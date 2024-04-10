use crate::{
    codec::{base_bitrate, codec_thread_num, EncoderApi, EncoderCfg, Quality as Q},
    hw, CodecFormat, EncodeInput, ImageFormat, ImageRgb, Pixfmt, HW_STRIDE_ALIGN,
};
use hbb_common::{
    allow_err,
    anyhow::{anyhow, bail, Context},
    bytes::Bytes,
    config::HwCodecConfig,
    log,
    message_proto::{EncodedVideoFrame, EncodedVideoFrames, VideoFrame},
    ResultType,
};
use hwcodec::{
    common::DataFormat,
    ffmpeg::AVPixelFormat,
    ffmpeg_ram::{
        decode::{DecodeContext, DecodeFrame, Decoder},
        encode::{EncodeContext, EncodeFrame, Encoder},
        CodecInfo, CodecInfos,
        Quality::{self, *},
        RateControl::{self, *},
    },
};

const CFG_KEY_ENCODER: &str = "bestHwEncoders";
const CFG_KEY_DECODER: &str = "bestHwDecoders";

const DEFAULT_PIXFMT: AVPixelFormat = AVPixelFormat::AV_PIX_FMT_NV12;
pub const DEFAULT_TIME_BASE: [i32; 2] = [1, 30];
const DEFAULT_GOP: i32 = i32::MAX;
const DEFAULT_HW_QUALITY: Quality = Quality_Default;
const DEFAULT_RC: RateControl = RC_DEFAULT;

#[derive(Debug, Clone)]
pub struct HwRamEncoderConfig {
    pub name: String,
    pub width: usize,
    pub height: usize,
    pub quality: Q,
    pub keyframe_interval: Option<usize>,
}

pub struct HwRamEncoder {
    encoder: Encoder,
    name: String,
    pub format: DataFormat,
    pub pixfmt: AVPixelFormat,
    width: u32,
    height: u32,
    bitrate: u32, //kbs
}

impl EncoderApi for HwRamEncoder {
    fn new(cfg: EncoderCfg, _i444: bool) -> ResultType<Self>
    where
        Self: Sized,
    {
        match cfg {
            EncoderCfg::HWRAM(config) => {
                let b = Self::convert_quality(config.quality);
                let base_bitrate = base_bitrate(config.width as _, config.height as _);
                let mut bitrate = base_bitrate * b / 100;
                if base_bitrate <= 0 {
                    bitrate = base_bitrate;
                }
                let gop = config.keyframe_interval.unwrap_or(DEFAULT_GOP as _) as i32;
                let ctx = EncodeContext {
                    name: config.name.clone(),
                    width: config.width as _,
                    height: config.height as _,
                    pixfmt: DEFAULT_PIXFMT,
                    align: HW_STRIDE_ALIGN as _,
                    bitrate: bitrate as i32 * 1000,
                    timebase: DEFAULT_TIME_BASE,
                    gop,
                    quality: DEFAULT_HW_QUALITY,
                    rc: DEFAULT_RC,
                    thread_count: codec_thread_num(16) as _, // ffmpeg's thread_count is used for cpu
                };
                let format = match Encoder::format_from_name(config.name.clone()) {
                    Ok(format) => format,
                    Err(_) => {
                        return Err(anyhow!(format!(
                            "failed to get format from name:{}",
                            config.name
                        )))
                    }
                };
                match Encoder::new(ctx.clone()) {
                    Ok(encoder) => Ok(HwRamEncoder {
                        encoder,
                        name: config.name,
                        format,
                        pixfmt: ctx.pixfmt,
                        width: ctx.width as _,
                        height: ctx.height as _,
                        bitrate,
                    }),
                    Err(_) => {
                        HwCodecConfig::clear();
                        Err(anyhow!(format!("Failed to create encoder")))
                    }
                }
            }
            _ => Err(anyhow!("encoder type mismatch")),
        }
    }

    fn encode_to_message(&mut self, input: EncodeInput, _ms: i64) -> ResultType<VideoFrame> {
        let mut vf = VideoFrame::new();
        let mut frames = Vec::new();
        for frame in self
            .encode(input.yuv()?)
            .with_context(|| "Failed to encode")?
        {
            frames.push(EncodedVideoFrame {
                data: Bytes::from(frame.data),
                pts: frame.pts as _,
                key: frame.key == 1,
                ..Default::default()
            });
        }
        if frames.len() > 0 {
            let frames = EncodedVideoFrames {
                frames: frames.into(),
                ..Default::default()
            };
            match self.format {
                DataFormat::H264 => vf.set_h264s(frames),
                DataFormat::H265 => vf.set_h265s(frames),
                _ => bail!("unsupported format: {:?}", self.format),
            }
            Ok(vf)
        } else {
            Err(anyhow!("no valid frame"))
        }
    }

    fn yuvfmt(&self) -> crate::EncodeYuvFormat {
        let pixfmt = if self.pixfmt == AVPixelFormat::AV_PIX_FMT_NV12 {
            Pixfmt::NV12
        } else {
            Pixfmt::I420
        };
        let stride = self
            .encoder
            .linesize
            .clone()
            .drain(..)
            .map(|i| i as usize)
            .collect();
        crate::EncodeYuvFormat {
            pixfmt,
            w: self.encoder.ctx.width as _,
            h: self.encoder.ctx.height as _,
            stride,
            u: self.encoder.offset[0] as _,
            v: if pixfmt == Pixfmt::NV12 {
                0
            } else {
                self.encoder.offset[1] as _
            },
        }
    }

    #[cfg(feature = "vram")]
    fn input_texture(&self) -> bool {
        false
    }

    fn set_quality(&mut self, quality: crate::codec::Quality) -> ResultType<()> {
        let b = Self::convert_quality(quality);
        let bitrate = base_bitrate(self.width as _, self.height as _) * b / 100;
        if bitrate > 0 {
            self.encoder.set_bitrate((bitrate * 1000) as _).ok();
            self.bitrate = bitrate;
        }
        Ok(())
    }

    fn bitrate(&self) -> u32 {
        self.bitrate
    }

    fn support_abr(&self) -> bool {
        !self.name.contains("qsv")
    }
}

impl HwRamEncoder {
    pub fn best() -> CodecInfos {
        get_config(CFG_KEY_ENCODER).unwrap_or(CodecInfos {
            h264: None,
            h265: None,
        })
    }

    pub fn encode(&mut self, yuv: &[u8]) -> ResultType<Vec<EncodeFrame>> {
        match self.encoder.encode(yuv) {
            Ok(v) => {
                let mut data = Vec::<EncodeFrame>::new();
                data.append(v);
                Ok(data)
            }
            Err(_) => Ok(Vec::<EncodeFrame>::new()),
        }
    }

    pub fn convert_quality(quality: crate::codec::Quality) -> u32 {
        use crate::codec::Quality;
        match quality {
            Quality::Best => 150,
            Quality::Balanced => 100,
            Quality::Low => 50,
            Quality::Custom(b) => b,
        }
    }
}

pub struct HwRamDecoder {
    decoder: Decoder,
    pub info: CodecInfo,
}

impl HwRamDecoder {
    pub fn best() -> CodecInfos {
        get_config(CFG_KEY_DECODER).unwrap_or(CodecInfos {
            h264: None,
            h265: None,
        })
    }

    pub fn new(format: CodecFormat) -> ResultType<Self> {
        log::info!("try create {format:?} ram decoder");
        let best = HwRamDecoder::best();
        let info = match format {
            CodecFormat::H264 => {
                if let Some(info) = best.h264 {
                    info
                } else {
                    bail!("no h264 decoder, should not be here");
                }
            }
            CodecFormat::H265 => {
                if let Some(info) = best.h265 {
                    info
                } else {
                    bail!("no h265 decoder, should not be here");
                }
            }
            _ => bail!("unsupported format: {:?}", format),
        };
        let ctx = DecodeContext {
            name: info.name.clone(),
            device_type: info.hwdevice.clone(),
            thread_count: codec_thread_num(16) as _,
        };
        match Decoder::new(ctx) {
            Ok(decoder) => Ok(HwRamDecoder { decoder, info }),
            Err(_) => {
                HwCodecConfig::clear();
                Err(anyhow!(format!("Failed to create decoder")))
            }
        }
    }
    pub fn decode(&mut self, data: &[u8]) -> ResultType<Vec<HwRamDecoderImage>> {
        match self.decoder.decode(data) {
            Ok(v) => Ok(v.iter().map(|f| HwRamDecoderImage { frame: f }).collect()),
            Err(e) => Err(anyhow!(e)),
        }
    }
}

pub struct HwRamDecoderImage<'a> {
    frame: &'a DecodeFrame,
}

impl HwRamDecoderImage<'_> {
    // rgb [in/out] fmt and stride must be set in ImageRgb
    pub fn to_fmt(&self, rgb: &mut ImageRgb, i420: &mut Vec<u8>) -> ResultType<()> {
        let frame = self.frame;
        rgb.w = frame.width as _;
        rgb.h = frame.height as _;
        // take dst_stride into account when you convert
        let dst_stride = rgb.stride();
        match frame.pixfmt {
            AVPixelFormat::AV_PIX_FMT_NV12 => hw::hw_nv12_to(
                rgb.fmt(),
                frame.width as _,
                frame.height as _,
                &frame.data[0],
                &frame.data[1],
                frame.linesize[0] as _,
                frame.linesize[1] as _,
                &mut rgb.raw as _,
                i420,
                HW_STRIDE_ALIGN,
            )?,
            AVPixelFormat::AV_PIX_FMT_YUV420P => {
                hw::hw_i420_to(
                    rgb.fmt(),
                    frame.width as _,
                    frame.height as _,
                    &frame.data[0],
                    &frame.data[1],
                    &frame.data[2],
                    frame.linesize[0] as _,
                    frame.linesize[1] as _,
                    frame.linesize[2] as _,
                    &mut rgb.raw as _,
                )?;
            }
        }
        Ok(())
    }

    pub fn bgra(&self, bgra: &mut Vec<u8>, i420: &mut Vec<u8>) -> ResultType<()> {
        let mut rgb = ImageRgb::new(ImageFormat::ARGB, 1);
        self.to_fmt(&mut rgb, i420)?;
        *bgra = rgb.raw;
        Ok(())
    }

    pub fn rgba(&self, rgba: &mut Vec<u8>, i420: &mut Vec<u8>) -> ResultType<()> {
        let mut rgb = ImageRgb::new(ImageFormat::ABGR, 1);
        self.to_fmt(&mut rgb, i420)?;
        *rgba = rgb.raw;
        Ok(())
    }
}

fn get_config(k: &str) -> ResultType<CodecInfos> {
    let v = HwCodecConfig::load()
        .options
        .get(k)
        .unwrap_or(&"".to_owned())
        .to_owned();
    match CodecInfos::deserialize(&v) {
        Ok(v) => Ok(v),
        Err(_) => Err(anyhow!("Failed to get config:{}", k)),
    }
}

pub fn check_available_hwcodec() {
    let ctx = EncodeContext {
        name: String::from(""),
        width: 1920,
        height: 1080,
        pixfmt: DEFAULT_PIXFMT,
        align: HW_STRIDE_ALIGN as _,
        bitrate: 0,
        timebase: DEFAULT_TIME_BASE,
        gop: DEFAULT_GOP,
        quality: DEFAULT_HW_QUALITY,
        rc: DEFAULT_RC,
        thread_count: 4,
    };
    let encoders = CodecInfo::score(Encoder::available_encoders(ctx));
    let decoders = CodecInfo::score(Decoder::available_decoders());

    if let Ok(old_encoders) = get_config(CFG_KEY_ENCODER) {
        if let Ok(old_decoders) = get_config(CFG_KEY_DECODER) {
            if encoders == old_encoders && decoders == old_decoders {
                return;
            }
        }
    }

    if let Ok(encoders) = encoders.serialize() {
        if let Ok(decoders) = decoders.serialize() {
            let mut config = HwCodecConfig::load();
            config.options.insert(CFG_KEY_ENCODER.to_owned(), encoders);
            config.options.insert(CFG_KEY_DECODER.to_owned(), decoders);
            config.store();
            return;
        }
    }
    log::error!("Failed to serialize codec info");
}

pub fn hwcodec_new_check_process() {
    use std::sync::Once;
    let f = || {
        // Clear to avoid checking process errors
        // But when the program is just started, the configuration file has not been updated, and the new connection will read an empty configuration
        // TODO: --server start multi times on windows startup, which will clear the last config and cause concurrent file writing
        HwCodecConfig::clear();
        if let Ok(exe) = std::env::current_exe() {
            if let Some(_) = exe.file_name().to_owned() {
                let arg = "--check-hwcodec-config";
                if let Ok(mut child) = std::process::Command::new(exe).arg(arg).spawn() {
                    // wait up to 30 seconds, it maybe slow on windows startup for poorly performing machines
                    for _ in 0..30 {
                        std::thread::sleep(std::time::Duration::from_secs(1));
                        if let Ok(Some(_)) = child.try_wait() {
                            break;
                        }
                    }
                    allow_err!(child.kill());
                    std::thread::sleep(std::time::Duration::from_millis(30));
                    match child.try_wait() {
                        Ok(Some(status)) => {
                            log::info!("Check hwcodec config, exit with: {status}")
                        }
                        Ok(None) => {
                            log::info!(
                                "Check hwcodec config, status not ready yet, let's really wait"
                            );
                            let res = child.wait();
                            log::info!("Check hwcodec config, wait result: {res:?}");
                        }
                        Err(e) => {
                            log::error!("Check hwcodec config, error attempting to wait: {e}")
                        }
                    }
                }
            }
        };
    };
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        std::thread::spawn(f);
    });
}
