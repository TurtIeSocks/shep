//! `shep import`: reading a pm2 dump into a Flockfile.
//!
//! [`dump`] parses a `dump.pm2` document into rows; [`convert`] collapses
//! those rows into apps and maps every field this importer knows how to
//! map. Filtering inherited environment noise out of declared config, and
//! rendering the result plus the `import` verb itself, build on top of what
//! these two modules already do.

pub(crate) mod convert;
pub(crate) mod dump;
