// Version 0.1.0 of a module that keeps its state in tables of its own.
module.exports = {
  sc_plugin_api_version: 1,
  tables: [
    {
      name: "tm_notebooks",
      label: "Notebooks",
      fields: [
        { name: "owner", references: "users" },
        { name: "title", type: "text", required: true },
      ],
      ownership: "owner === user.id",
    },
    {
      name: "tm_notes",
      fields: [
        { name: "notebook", references: "tm_notebooks", required: true },
        { name: "body", type: "text" },
      ],
      ownership: "notebookⱵowner === user.id",
    },
  ],
};
