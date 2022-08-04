use anyhow::{bail, Result as AnyResult};
use bytes::Bytes;
use image::imageops::FilterType;
use image::io::Reader as ImageReader;
use image::{GenericImageView, ImageOutputFormat};
use log::{error, info, warn};
use std::fmt::Display;
use std::io::Cursor;
use std::path::Path;
use std::process::Stdio;
use teloxide::net::Download;
use teloxide::prelude::*;
use teloxide::types::{File as TgFile, InputFile};
use tempfile::NamedTempFile;
use tokio::fs::File;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use webp::Encoder as WebpEncoder;

const MAX_SIZE: u32 = 10 << 20;
const MAX_OUTPUT_WEBM_SIZE: usize = 256 * 1000;

const FFMPEG: &str = "ffmpeg";

const FFMPEG_ARGS: (&[&str], &[&str]) = (
    &["-hide_banner", "-t", "3", "-i"],
    &[
        "-vf",
        "scale=w=512:h=512:force_original_aspect_ratio=decrease",
        "-c:v",
        "libvpx-vp9",
        "-f",
        "webm",
        "-an",
        "-",
    ],
);

#[derive(Debug)]
struct BadRequest {
    msg: &'static str,
}

impl Display for BadRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.msg)
    }
}

impl BadRequest {
    fn new(msg: &'static str) -> Self {
        Self { msg }
    }
}

async fn process_image(file: Vec<u8>) -> AnyResult<(Bytes, &'static str)> {
    match ImageReader::new(Cursor::new(file))
        .with_guessed_format()
        .unwrap()
        .decode()
    {
        Ok(img) => {
            info!("got img of {:?}", img.dimensions());
            let img = img.resize(512, 512, FilterType::Lanczos3);
            // webp::Encoder sometimes fails with Unimplemented when inputting small images.
            return Ok(match WebpEncoder::from_image(&img) {
                Ok(webp) => {
                    let mem = webp.encode_lossless();
                    (Bytes::copy_from_slice(&*mem), "webp")
                }
                Err(e) => {
                    warn!("webp: {}, falling back to png", e);
                    let mut v = Cursor::new(Vec::with_capacity(60000));
                    img.write_to(&mut v, ImageOutputFormat::Png)?;
                    (v.into_inner().into(), "png")
                }
            });
        }
        Err(e) => {
            info!("decode failed: {}", e);
            bail!(BadRequest::new("File is not an image!"))
        }
    }
}

// Passing a mp4 video from pipe sometimes causes failure in codecs detection of ffmpeg, so we have
// to use a temporary file.
async fn process_video(file: &Path) -> AnyResult<(Bytes, &'static str)> {
    // FIXME: output could be still too big even when lossy, try specify a bit rate?
    // FIXME: current implementation often has to run ffmpeg twice, try to avoid the lossless
    //        attempt in such cases.

    let mut lossy = false;
    loop {
        let mut cmd = Command::new(FFMPEG);
        let mut cmd = cmd.args(FFMPEG_ARGS.0).arg(file);
        if !lossy {
            cmd = cmd.arg("-lossless").arg("1");
        }
        let out = cmd
            .args(FFMPEG_ARGS.1)
            .stdout(Stdio::piped())
            .spawn()?
            .wait_with_output()
            .await?;

        if !out.status.success() {
            error!("ffmpeg failed: {:?}", out.status);
            bail!("ffmpeg")
        }
        if !lossy && out.stdout.len() > MAX_OUTPUT_WEBM_SIZE {
            lossy = true;
            info!("retrying with lossy");
        } else {
            return Ok((Bytes::from(out.stdout), "webm"));
        }
    }
}

async fn handle_image(f: TgFile, bot: &AutoSend<Bot>) -> AnyResult<(Bytes, &'static str)> {
    let mut v = Vec::with_capacity(f.file_size as usize);
    bot.download_file(&f.file_path, &mut v).await?;
    info!("downloaded {} bytes", v.len());
    process_image(v).await
}

async fn handle_video(f: TgFile, bot: &AutoSend<Bot>) -> AnyResult<(Bytes, &'static str)> {
    let path = NamedTempFile::new()?.into_temp_path();
    let mut tmp = File::create(&path).await?;
    bot.download_file(&f.file_path, &mut tmp).await?;
    tmp.flush().await?;
    drop(tmp);
    info!("downloaded {} bytes", f.file_size);
    process_video(&path).await
}

async fn handle_media(
    file_id: &String,
    bot: &AutoSend<Bot>,
    is_video: bool,
) -> AnyResult<(Bytes, &'static str)> {
    let f = bot.get_file(file_id).await?;
    if f.file_size > MAX_SIZE {
        bail!("File too big")
    }
    if is_video {
        handle_video(f, bot).await
    } else {
        handle_image(f, bot).await
    }
}

async fn handler(msg: Message, bot: &AutoSend<Bot>) -> &'static str {
    let ch = &msg.chat;
    info!(
        "from {} {} (@{} {})",
        ch.first_name().unwrap_or(""),
        ch.last_name().unwrap_or(""),
        ch.username().unwrap_or(""),
        ch.id.0
    );
    let mut is_video = false;
    let (file_id, size, file_name) = if let Some(doc) = msg.document() {
        info!(
            "downloading document {} of {} bytes",
            doc.file_name.as_deref().unwrap_or(""),
            doc.file_size
        );
        if let Some(s) = &doc.file_name {
            is_video = s.ends_with(".gif");
        }
        (&doc.file_id, doc.file_size, &doc.file_name)
    } else if let Some(sizes) = msg.photo() {
        let ph = sizes
            .iter()
            .find(|ph| ph.width >= 512 || ph.height >= 512)
            .unwrap_or_else(|| sizes.last().unwrap());
        info!(
            "downloading photo of {} {}, {} bytes",
            ph.width, ph.height, ph.file_size
        );
        (&ph.file_id, ph.file_size, &None)
    } else if let Some(ani) = msg.animation() {
        info!(
            "downloading animation {} of {}, {}, {}, {} bytes",
            ani.file_name.as_deref().unwrap_or(""),
            ani.width,
            ani.height,
            ani.duration,
            ani.file_size
        );
        is_video = true;
        (&ani.file_id, ani.file_size, &ani.file_name)
    } else if Some("/start") == msg.text() {
        return "Hi! Send me an image or a GIF animation, and I'll convert it for use with @Stickers.";
    } else {
        info!("invalid: {:#?}", msg);
        return "Please send an image or an GIF animation.";
    };

    if size > MAX_SIZE {
        return "File is too big.";
    }

    match handle_media(file_id, bot, is_video).await {
        Ok((f, suf)) => {
            let n = f.len();
            let f = InputFile::memory(f);
            let mut out_name;
            if let Some(s) = file_name {
                // s.replace('.', "_")
                out_name = s.clone();
                out_name.push('.');
            } else {
                out_name = "out.".to_owned();
            };
            out_name.push_str(suf);
            let f = f.file_name(out_name);
            info!("sending {} bytes", n);
            if let Err(e) = bot.send_document(msg.chat.id, f).await {
                error!("send_document: {:?}", e);
                return "Failed to send file.";
            }
        }
        Err(e) => {
            error!("handle: {:?}", e);
            return if let Ok(e) = e.downcast::<BadRequest>() {
                e.msg
            } else {
                "Something went wrong."
            };
        }
    }
    ""
}

#[tokio::main]
async fn main() {
    if std::env::var("RUST_LOG").is_err() {
        std::env::set_var("RUST_LOG", "info");
    }
    pretty_env_logger::init();
    info!("starting bot...");

    let bot = Bot::from_env().auto_send();
    info!("bot started by {:?}", bot.inner().client());

    teloxide::repl(bot, |msg: Message, bot: AutoSend<Bot>| async move {
        tokio::spawn(async move {
            let id = msg.chat.id;
            let s = handler(msg, &bot).await;
            if !s.is_empty() {
                if let Err(e) = bot.send_message(id, s).await {
                    error!("send_message: {:?}", e);
                }
            }
        });
        // TODO: join the spawned tasks when interrupted?
        respond(())
    })
    .await;
}
