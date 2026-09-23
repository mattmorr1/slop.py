use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static FIXTURE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

pub(crate) struct TempFixture(PathBuf);

impl TempFixture {
    pub(crate) fn new(name: &str) -> Self {
        let source = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures")
            .join(name);
        let sequence = FIXTURE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let destination = std::env::temp_dir().join(format!(
            "slop-fixture-{name}-{}-{sequence}",
            std::process::id()
        ));
        copy_sources(&source, &destination).expect("copy fixture sources");
        let index = destination.join("index.scip");
        std::fs::copy(source.join("index.scip"), &index)
            .expect("copy fixture index after sources");
        crate::index::write_stamp(&destination, &index);
        Self(destination)
    }
}

fn copy_sources(source: &Path, destination: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(destination)?;
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        let name = entry.file_name();
        if name == "index.scip" || name == "index.scip.stamp" {
            continue;
        }
        if entry.file_type()?.is_dir() {
            copy_sources(&entry.path(), &destination.join(name))?;
        } else {
            std::fs::copy(entry.path(), destination.join(name))?;
        }
    }
    Ok(())
}

impl Deref for TempFixture {
    type Target = Path;

    fn deref(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempFixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
