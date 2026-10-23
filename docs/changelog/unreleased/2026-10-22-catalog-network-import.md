## Import

- Restored original-file probing and adjacent XMP reads during Lightroom catalog import. Skipping those reads could omit camera and lens information needed to interpret edits. Import fidelity takes priority over the catalog-only speed optimization; network imports may take longer again. This restores the previous import behavior, not identical rendering to Lightroom.
