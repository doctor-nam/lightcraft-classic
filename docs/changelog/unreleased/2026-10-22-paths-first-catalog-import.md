## Library

- **Paths first during catalog import:** publish and allow selection of all source folders before develop/history tables and original probes. Keep pending folders transient and retain only committed photos on cancellation or failure.
- **Network import overhead:** overlap original existence, XMP reads and stamp checks on a bounded pool; suppress repeated sidebar disk scans during import. Preserve full probing and XMP ownership/fallback. Real Samba throughput is not yet measured. Sources: own design.
