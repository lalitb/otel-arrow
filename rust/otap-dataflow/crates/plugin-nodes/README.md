# otap-df-plugin-nodes

Adapters that bridge loaded plugins into the engine runtime registry:
`WasmProcessorAdapter`, `WasmExporterAdapter`, and the dynamic factory
builders consumed by `engine::PipelineFactory::build_with_dynamic`.

See [`docs/dynamic-processor-exporter-plugins-phase1.md`](../../docs/dynamic-processor-exporter-plugins-phase1.md)
and the [operator usage guide](../../docs/dynamic-plugins-usage.md).
