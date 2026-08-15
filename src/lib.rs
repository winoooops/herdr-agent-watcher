pub mod daemon;
pub mod sidebar;

#[cfg(all(feature = "runtime", unix))]
pub mod agent;
#[cfg(all(feature = "runtime", unix))]
pub mod agents;
#[cfg(all(feature = "runtime", unix))]
pub mod aliases;
#[cfg(all(feature = "runtime", unix))]
pub mod debug;
#[cfg(all(feature = "runtime", unix))]
pub mod filesystem;
#[cfg(all(feature = "runtime", unix))]
pub mod herdr;
#[cfg(all(feature = "runtime", unix))]
pub mod runtime;
#[cfg(all(feature = "runtime", unix))]
pub mod terminal;

#[cfg(all(feature = "runtime", not(unix)))]
compile_error!("the runtime feature requires a Unix target");
