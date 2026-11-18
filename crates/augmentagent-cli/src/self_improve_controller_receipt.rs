//! Versioned controller-origin migration under the existing controller lock.
use super::*;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};

pub(super) fn read(path: &Path) -> Result<Ownership> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let meta = file.metadata()?;
    anyhow::ensure!(
        meta.is_file()
            && meta.nlink() == 1
            && meta.uid() == unsafe { libc::geteuid() }
            && meta.mode() & 0o077 == 0
            && meta.len() <= 16384,
        "invalid controller receipt"
    );
    Ok(serde_json::from_reader(file)?)
}

pub(super) fn migration(
    recorded: &Ownership,
    current: &Ownership,
    origin: &str,
) -> Result<Option<Ownership>> {
    let mut expected = current.clone();
    expected.previous_origin_sha256 = recorded.previous_origin_sha256.clone();
    expected.previous_identity_sha256 = recorded.previous_identity_sha256.clone();
    if recorded.version == 1
        && current.version == 2
        && recorded.previous_origin_sha256.is_none()
        && recorded.previous_identity_sha256.is_none()
    {
        let policy = repository_identity::Policy::from_env()?;
        let canonical = policy
            .origin(origin)?
            .context("controller repository identity unavailable")?;
        anyhow::ensure!(
            policy.legacy_hash_matches(&canonical, origin, &recorded.origin_sha256),
            "controller ownership changed; preserved"
        );
        // Only the schema/origin witness may differ. Physical checkout,
        // primary Git repository and pointer bytes retain exact continuity.
        expected.version = 1;
        expected.origin_sha256 = recorded.origin_sha256.clone();
        anyhow::ensure!(
            recorded == &expected,
            "controller ownership changed; preserved"
        );
        let mut migrated = current.clone();
        migrated.previous_origin_sha256 = Some(recorded.origin_sha256.clone());
        return Ok(Some(migrated));
    }
    if recorded.version == 2
        && current.version == 2
        && recorded.origin_sha256 != current.origin_sha256
    {
        let policy = repository_identity::Policy::from_env()?;
        let canonical = policy
            .origin(origin)?
            .context("controller repository identity unavailable")?;
        anyhow::ensure!(
            policy.identity_hash_matches(&canonical, &recorded.origin_sha256),
            "controller ownership changed; preserved"
        );
        expected.origin_sha256 = recorded.origin_sha256.clone();
        anyhow::ensure!(
            recorded == &expected,
            "controller ownership changed; preserved"
        );
        let mut migrated = current.clone();
        migrated.previous_origin_sha256 = recorded.previous_origin_sha256.clone();
        migrated.previous_identity_sha256 = Some(recorded.origin_sha256.clone());
        return Ok(Some(migrated));
    }
    anyhow::ensure!(
        recorded == &expected,
        "controller ownership changed; preserved"
    );
    Ok(None)
}

pub(super) fn commit(path: &Path, previous: &Ownership, migrated: &Ownership) -> Result<()> {
    use std::io::Write;
    let parent = path
        .parent()
        .context("controller receipt parent unavailable")?;
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    file.as_file()
        .set_permissions(std::fs::Permissions::from_mode(0o600))?;
    file.write_all(&serde_json::to_vec(migrated)?)?;
    file.as_file().sync_all()?;
    anyhow::ensure!(
        &read(path)? == previous,
        "controller receipt changed during migration; preserved"
    );
    file.persist(path).map_err(|error| error.error)?;
    std::fs::File::open(parent)?.sync_all()?;
    tracing::info!(
        version = 2,
        receipt = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("controller"),
        legacy_url_witness = migrated.previous_origin_sha256.is_some(),
        previous_identity_witness = migrated.previous_identity_sha256.is_some(),
        "migrated controller repository identity receipt"
    );
    Ok(())
}
