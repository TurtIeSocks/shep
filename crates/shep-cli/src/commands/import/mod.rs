//! `shep import`: reading a pm2 dump into a Flockfile.
//!
//! [`dump`] parses a `dump.pm2` document into rows; [`mod@env`] splits a
//! row's environment into what a Flockfile should carry and what the
//! operator has to decide about; [`convert`] collapses rows into apps,
//! wiring `env`'s split into every field this importer knows how to map.
//! Rendering the result plus the `import` verb itself build on top of what
//! these three modules already do.

pub(crate) mod convert;
pub(crate) mod dump;
pub(crate) mod env;
