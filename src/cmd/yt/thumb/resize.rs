use std::path::PathBuf;
use tracing::{debug, info};
use xshell::{Shell, cmd};

const MAX_SIZE: u64 = 2 * 1024 * 1024; // 2 MB

pub fn run(file: &PathBuf) -> anyhow::Result<()> {
    if !file.exists() {
        anyhow::bail!("File does not exist: {}", file.display());
    }

    let original_size = std::fs::metadata(file)?.len();
    info!(
        "Original size: {:.2} MB",
        original_size as f64 / 1024.0 / 1024.0
    );

    if original_size < MAX_SIZE {
        info!("File is already under 2MB, no action needed");
        return Ok(());
    }

    let sh = Shell::new()?;
    let temp_file = file.with_extension("tmp.resized");

    let file_str = file
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("Invalid file path"))?;
    let temp_str = temp_file
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("Invalid temp path"))?;

    let mut scale = 100u32;
    loop {
        let scale_arg = format!("{}%", scale);

        debug!("Resizing at {}%", scale);
        cmd!(sh, "magick {file_str} -resize {scale_arg} {temp_str}").run()?;

        let new_size = std::fs::metadata(&temp_file)?.len();
        debug!(
            "Size at {}%: {:.2} MB",
            scale,
            new_size as f64 / 1024.0 / 1024.0
        );

        if new_size < MAX_SIZE {
            std::fs::rename(&temp_file, file)?;
            info!(
                "Done: {:.2} MB -> {:.2} MB (at {}% scale)",
                original_size as f64 / 1024.0 / 1024.0,
                new_size as f64 / 1024.0 / 1024.0,
                scale
            );
            return Ok(());
        }

        if scale <= 10 {
            std::fs::remove_file(&temp_file)?;
            anyhow::bail!("Could not reduce file size below 2MB even at 10% scale");
        }

        scale -= 3;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_resize_reduces_large_image() {
        let sh = Shell::new().unwrap();
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.png");
        let path_str = path.to_str().unwrap();

        cmd!(sh, "magick -size 600x600 xc: +noise Random {path_str}")
            .run()
            .unwrap();

        assert!(std::fs::metadata(&path).unwrap().len() >= MAX_SIZE);

        run(&path).unwrap();

        assert!(std::fs::metadata(&path).unwrap().len() < MAX_SIZE);
    }

    #[test]
    fn test_small_image_unchanged() {
        let sh = Shell::new().unwrap();
        let dir = tempdir().unwrap();
        let path = dir.path().join("small.png");
        let path_str = path.to_str().unwrap();

        cmd!(sh, "magick -size 100x100 xc:red {path_str}")
            .run()
            .unwrap();
        let original_size = std::fs::metadata(&path).unwrap().len();

        run(&path).unwrap();

        assert_eq!(std::fs::metadata(&path).unwrap().len(), original_size);
    }
}
