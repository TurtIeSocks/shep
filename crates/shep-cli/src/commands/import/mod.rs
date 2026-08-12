//! `shep import`: reading a pm2 dump into a Flockfile.
//!
//! [`dump`] parses a `dump.pm2` document into rows. Collapsing the instances
//! of one app into a single entry, mapping fields onto a Flockfile, filtering
//! inherited environment noise out of declared config, and rendering the
//! result plus the `import` verb itself all build on top of what this module
//! reads.

pub(crate) mod dump;
