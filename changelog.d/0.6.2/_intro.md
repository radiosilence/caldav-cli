### Changed

- **Release builds now use thin LTO with 16 codegen units instead of fat LTO with 1.**
  Fat LTO with a single codegen unit was the slowest possible build configuration,
  serialising whole-program optimisation across the entire dependency tree. For an
  I/O-bound tool like caldav-cli, the runtime binary gains nothing measurable from
  this slow path. Thin LTO with concurrent codegen dramatically speeds iteration
  while still providing the performance LTO is installed to deliver.
