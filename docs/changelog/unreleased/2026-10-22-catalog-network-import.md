## Performance

- Lightroom catalog import registers photos using catalog metadata without reading and hashing every original. This avoids transferring the entire photo library from a NAS or cloud folder. Original availability is still checked; metadata absent from the catalog and content hashes are not populated during import.
