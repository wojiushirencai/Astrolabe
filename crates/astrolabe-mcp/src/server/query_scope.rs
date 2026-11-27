//! Per-call requested directory, independent of substring path filters.
//! The guard restores the previous scope for nested calls and worker reuse.
use std::{
    cell::RefCell,
    path::{Path, PathBuf},
};
thread_local! { static PREFIX: RefCell<Option<PathBuf>> = const { RefCell::new(None) }; }
pub(crate) struct Guard(Option<PathBuf>);
impl Drop for Guard {
    fn drop(&mut self) {
        PREFIX.with(|slot| *slot.borrow_mut() = self.0.take());
    }
}
pub(crate) fn enter(requested: Option<&str>, active: &str) -> Guard {
    let prefix = requested
        .filter(|s| !s.trim().is_empty())
        .and_then(|raw| crate::root::resolve_call_index_root(raw).ok())
        .and_then(|requested| {
            astrolabe_core::paths::strip_prefix_normalized(Path::new(active), &requested)
        })
        .filter(|p| !p.as_os_str().is_empty());
    Guard(PREFIX.with(|slot| slot.replace(prefix)))
}
pub(crate) fn allows(path: &str) -> bool {
    PREFIX.with(|slot| {
        slot.borrow()
            .as_ref()
            .is_none_or(|p| Path::new(path).starts_with(p))
    })
}
pub(crate) fn active() -> bool {
    PREFIX.with(|slot| slot.borrow().is_some())
}
pub(crate) fn key() -> String {
    PREFIX.with(|slot| {
        slot.borrow()
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default()
    })
}
pub(crate) fn matches_glob(set: Option<&globset::GlobSet>, path: &str) -> bool {
    PREFIX.with(|slot| {
        let prefix = slot.borrow();
        let relative = match prefix.as_ref() {
            Some(prefix) => match Path::new(path).strip_prefix(prefix) {
                Ok(p) => p,
                Err(_) => return false,
            },
            None => Path::new(path),
        };
        set.is_none_or(|set| set.is_match(relative))
    })
}
