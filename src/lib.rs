pub mod compat;
pub mod daemon;
pub mod ipc;
pub mod sidebar;

#[cfg(test)]
pub(crate) mod test_env;

#[cfg(feature = "runtime")]
pub mod agent;
#[cfg(feature = "runtime")]
pub mod agents;
#[cfg(feature = "runtime")]
pub mod aliases;
#[cfg(feature = "runtime")]
pub mod debug;
#[cfg(feature = "runtime")]
pub mod filesystem;
#[cfg(feature = "runtime")]
pub mod herdr;
#[cfg(feature = "runtime")]
pub mod runtime;
#[cfg(feature = "runtime")]
pub mod terminal;
