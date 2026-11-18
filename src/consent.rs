//! The user's permission for dictation to use the microphone: a file under
//! the user's configuration, written when they allow it.

use std::{
    fs::OpenOptions,
    io::{Read, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::Path,
};

/// Whether the user allowed microphone use.
pub(crate) fn given(file: Option<&Path>) -> bool {
    file.is_some_and(|path| {
        let Ok(file) = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(path)
        else {
            return false;
        };
        if !file
            .metadata()
            .is_ok_and(|metadata| metadata.is_file() && metadata.nlink() == 1)
        {
            return false;
        }
        let mut text = String::new();
        file.take(65).read_to_string(&mut text).is_ok()
            && text.len() <= 64
            && text.trim() == "microphone allowed"
    })
}

/// Records that the user allowed microphone use.
pub(crate) fn grant(file: Option<&Path>) -> std::io::Result<()> {
    let file = file.ok_or_else(|| std::io::Error::other("no configuration folder"))?;
    if let Some(parent) = file.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut marker = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(file)?;
    let metadata = marker.metadata()?;
    if !metadata.is_file() || metadata.nlink() != 1 {
        return Err(std::io::Error::other("invalid microphone permission file"));
    }
    marker.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    marker.set_len(0)?;
    marker.write_all(b"microphone allowed\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn consent_is_absent_until_granted() {
        let dir = std::env::temp_dir().join(format!("dictation-consent-{}", std::process::id()));
        let file = dir.join("enzimos/dictation-consent");
        assert!(!given(Some(&file)));
        assert!(!given(None));
        grant(Some(&file)).unwrap();
        assert!(given(Some(&file)));
        assert_eq!(
            std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o600
        );
        std::fs::write(&file, "something else").unwrap();
        assert!(!given(Some(&file)), "only the recorded permission counts");
        let guessed = file.with_file_name("guessed");
        std::os::unix::fs::symlink(&file, &guessed).unwrap();
        assert!(grant(Some(&guessed)).is_err());
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "something else");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
