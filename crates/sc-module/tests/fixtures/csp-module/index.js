// A module that suggests the content-security sources a provider's embedded
// form needs (TODO-payments P1.4). Suggesting changes nothing: an application
// allows them once its admin switches the suggestion on.

module.exports = {
  sc_plugin_api_version: 1,
  csp: {
    elements: {
      description: "The provider's embedded payment form",
      sources: {
        "script-src": ["https://js.provider.example"],
        "frame-src": ["https://js.provider.example"],
        "connect-src": ["https://api.provider.example"],
      },
    },
    // Not offered: a keyword is not a host.
    sloppy: {
      description: "Anything goes",
      sources: { "script-src": ["'unsafe-inline'"] },
    },
  },
};
