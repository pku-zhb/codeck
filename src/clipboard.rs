use std::fs::File;
use std::io::BufWriter;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use arboard::{Clipboard, Error as ClipboardError};

pub fn save_clipboard_image() -> Result<Option<PathBuf>> {
    let mut clipboard = Clipboard::new().context("open system clipboard")?;
    let image = match clipboard.get_image() {
        Ok(image) => image,
        Err(ClipboardError::ContentNotAvailable) => return Ok(None),
        Err(error) => return Err(error).context("read image from system clipboard"),
    };
    let expected_bytes = image
        .width
        .checked_mul(image.height)
        .and_then(|pixels| pixels.checked_mul(4))
        .context("clipboard image dimensions overflow")?;
    if image.width == 0 || image.height == 0 || image.bytes.len() != expected_bytes {
        bail!("clipboard image has invalid RGBA dimensions");
    }

    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "codex-deck-clipboard-{}-{timestamp}.png",
        std::process::id()
    ));
    write_rgba_png(
        &path,
        image.width as u32,
        image.height as u32,
        image.bytes.as_ref(),
    )?;
    Ok(Some(path))
}

pub fn image_paths_from_paste(text: &str) -> Vec<PathBuf> {
    let candidates = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(normalize_pasted_path)
        .collect::<Vec<_>>();
    if candidates.is_empty()
        || candidates
            .iter()
            .any(|path| !path.is_file() || !is_image_path(path))
    {
        Vec::new()
    } else {
        candidates
    }
}

fn normalize_pasted_path(text: &str) -> PathBuf {
    let unquoted = if text.len() >= 2
        && ((text.starts_with('"') && text.ends_with('"'))
            || (text.starts_with('\'') && text.ends_with('\'')))
    {
        &text[1..text.len() - 1]
    } else {
        text
    };
    PathBuf::from(unquoted.replace("\\ ", " "))
}

fn is_image_path(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| {
            matches!(
                extension.to_ascii_lowercase().as_str(),
                "png" | "jpg" | "jpeg" | "webp" | "gif"
            )
        })
        .unwrap_or(false)
}

fn write_rgba_png(path: &Path, width: u32, height: u32, rgba: &[u8]) -> Result<()> {
    let file = File::create(path).with_context(|| format!("create {}", path.display()))?;
    let mut encoder = png::Encoder::new(BufWriter::new(file), width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder
        .write_header()
        .context("write clipboard PNG header")?;
    writer
        .write_image_data(rgba)
        .context("write clipboard PNG pixels")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pasted_image_paths_are_detected_without_consuming_normal_text() {
        let path = std::env::temp_dir().join(format!(
            "codex-deck-pasted-image-{}.png",
            std::process::id()
        ));
        write_rgba_png(&path, 1, 1, &[255, 0, 0, 255]).expect("write PNG fixture");

        assert_eq!(
            image_paths_from_paste(&format!("'{}'", path.display())),
            vec![path.clone()]
        );
        assert!(image_paths_from_paste("please inspect image.png").is_empty());

        std::fs::remove_file(path).expect("remove PNG fixture");
    }
}
