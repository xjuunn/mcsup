mod adoptium;
mod azul;
mod foojay;
mod os_arch;
mod registry;

pub use registry::{JavaSourceRegistry, default_request, rank_sources};

pub(crate) use os_arch::{current_arch, current_os};
