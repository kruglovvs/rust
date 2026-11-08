# Dependency-only helper component. It carries the `overrides` entry in
# idf_component.yml (esp_codec_dev 1.6.2 for IDF 5.5.2) and forces the two
# pipeline entry points into the final image even though no Rust code calls them
# yet. Both make the build gate prove the component archives actually link.
idf_component_register()
