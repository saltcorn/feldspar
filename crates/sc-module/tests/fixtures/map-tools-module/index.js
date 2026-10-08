// Map tools are data (analytics TODO A5.12): a form, and the dataset
// operations its answers fill in. Built from the configuration, to show a
// function is called with it.
module.exports = {
  sc_plugin_api_version: 1,
  maptools: (configuration) => ({
    walk: {
      group: "Proximity",
      label: "Walking distance",
      description: "The area a walk of some minutes reaches.",
      params: [
        { name: "layer", label: "Layer", kind: "layer" },
        { name: "minutes", label: "Minutes", kind: "number", default: configuration.minutes || 10 },
      ],
      base: "layer",
      operations: [
        {
          kind: "calculated",
          params: { name: "walk", formula: "Geo.buffer({{geometry}}, {{minutes}} * 80)" },
        },
      ],
      layer: { geometry: { kind: "column", column: "walk" } },
    },
    broken: "not a tool",
  }),
};
