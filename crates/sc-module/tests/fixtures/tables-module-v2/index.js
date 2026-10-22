// Version 0.2.0: one field more on `tm_notes`, one table more, and a pair the
// notes must be unique on. Nothing removed.
module.exports = {
  sc_plugin_api_version: 1,
  tables: () => [
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
        { name: "pinned", type: "bool", required: true, default: false },
      ],
      unique: [["notebook", "body"]],
      ownership: "notebookⱵowner === user.id",
    },
    {
      name: "tm_tags",
      fields: [{ name: "note", references: "tm_notes" }, { name: "tag", type: "text" }],
    },
  ],
};
