use anyhow::Result;
use bytes::Bytes;
use image::imageops::FilterType;
use image::io::Reader as ImageReader;
use log::{error, info};
use std::io::Cursor;
use teloxide::net::Download;
use teloxide::prelude::*;
use teloxide::types::InputFile;
use webp::Encoder as WebpEncoder;

async fn handle(file_id: &String, bot: &AutoSend<Bot>) -> Result<Option<Bytes>> {
    let f = bot.get_file(file_id).await?;
    let mut v = Vec::with_capacity(f.file_size as usize);
    bot.download_file(&f.file_path, &mut v).await?;
    info!("downloaded {} bytes", f.file_size);
    Ok(
        match ImageReader::new(Cursor::new(v))
            .with_guessed_format()
            .unwrap()
            .decode()
        {
            Ok(img) => {
                info!("got img of {} {}", img.width(), img.height());
                let img = img.resize(512, 512, FilterType::Lanczos3);
                let webp = WebpEncoder::from_image(&img)
                    .map_err(|s: &str| anyhow::Error::msg(s.to_owned()))?;
                let mem = webp.encode_lossless();
                Some(Bytes::copy_from_slice(&*mem))
            }
            Err(e) => {
                info!("decode failed: {}", e);
                None
            }
        },
    )
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
        let ch = &msg.chat;
        let id = ch.id;
        info!(
            "from {} {} (@{} {})",
            ch.first_name().unwrap_or(""),
            ch.last_name().unwrap_or(""),
            ch.username().unwrap_or(""),
            id.0
        );
        let mut filename = None;
        let fileid = if let Some(doc) = msg.document() {
            if let Some(s) = &doc.file_name {
                info!("downloading {}", s);
                let mut s = s.replace('.', "_");
                s.push_str(".webp");
                filename = Some(s);
            }
            // TODO: reject huge files
            &doc.file_id
        } else if let Some(sizes) = msg.photo() {
            let ph = sizes
                .iter()
                .find(|ph| ph.width >= 512 || ph.height >= 512)
                .unwrap_or_else(|| sizes.last().unwrap());
            info!("downloading photo of {} {}", ph.width, ph.height);
            &ph.file_id
        } else {
            bot.send_message(id, "Please send an image file.").await?;
            return respond(());
        };

        match handle(fileid, &bot).await {
            Ok(f) => {
                if let Some(f) = f {
                    let n = f.len();
                    let f = InputFile::memory(f);
                    let f = if let Some(s) = filename {
                        f.file_name(s)
                    } else {
                        f.file_name("out.webp")
                    };
                    info!("sending {} bytes", n);
                    if let Err(e) = bot.send_document(id, f).await {
                        error!("send_document: {:?}", e);
                        bot.send_message(id, "Failed to send file.").await?;
                    }
                } else {
                    bot.send_message(id, "File is not an image.").await?;
                }
            }
            Err(e) => {
                error!("handle: {}", e);
                bot.send_message(id, "Something went wrong.").await?;
            }
        }
        respond(())
    })
    .await;
}
