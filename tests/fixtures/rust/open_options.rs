#[cfg(unix)]
fn check_file_with_unlinked(path: &Path, create: bool, allow_unlinked: bool) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(create)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    check_file_metadata(&file.metadata()?, allow_unlinked)
}
