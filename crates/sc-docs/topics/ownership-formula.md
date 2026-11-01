An ownership formula lets users read and change **some rows** of a table even though
their role is below the minimum roles set for it. It is a JavaScript expression that is
evaluated for each row and each operation. If it is true (or
[truthy](https://developer.mozilla.org/en-US/docs/Glossary/Truthy)), the logged-in user
owns the row and the operation is allowed.

The rule, in full: **allowed = the user's role meets the table's minimum role for the
operation, OR the formula is true for this row.** The formula only ever *adds* access. It
cannot take access away from a user whose role already meets the minimum, so it cannot be
used to hide rows from, say, staff who can read the whole table.

A formula is checked in the database wherever possible: it is translated to SQL, so a
user's query only ever sees the rows they own. Formulas that use something SQL cannot
express are evaluated row by row instead. An error while evaluating **denies**.

## What is in scope

#### Row fields

Every field of the row is in scope by its variable name{% if table %} — for
`{{ table.name }}`:

| Variable | Label | Type |
|---|---|---|
{% for f in table.fields %}
| `{{ f.name }}` | {{ f.label }} | {{ f.type }}{% if f.key_to %} → `{{ f.key_to.table }}`{% endif %} |
{% endfor %}
{% else %}.
{% endif %}

#### Fields of the rows a key points at (`Ⱶ`)

A key field can be followed to the row it points at by joining the two names with `Ⱶ`
(the "half H", U+2C75): `projectⱵmanager` is the `manager` field of the row that
`project` points at. Paths can be chained (`projectⱵclientⱵname`). If the key is
null, the whole path is null, so there is no need for `?.`.
{% set ns = namespace(keys=false) %}
{% for f in table.fields if f.key_to %}{% set ns.keys = true %}{% endfor %}
{% if ns.keys %}

On this table:

{% for f in table.fields if f.key_to %}
- `{{ f.name }}` → `{{ f.key_to.table }}`: {% for t in f.key_to.fields %}`{{ f.name }}Ⱶ{{ t.name }}`{% if not loop.last %}, {% endif %}{% endfor %}

{% endfor %}
{% elif table %}

`{{ table.name }}` has no key fields.
{% endif %}

#### Rows of other tables that point at this one (`Ↄ`)

The rows of a child table whose key points at this row are written as the child table's
name, `Ↄ` (the "reversed C", U+2183), and the key field: `sharesↃdocument` is the array
of `shares` rows whose `document` is this row. It supports `length`, `some`, `every`,
`filter`, `map`, `includes`, `join`, and `sum`, `min`, `max`, `avg`, `distinct`, `maxBy`,
`minBy`:

```js
sharesↃdocument.some(s => s.shared_with === user.id)
order_linesↃorder.filter(l => l.status === "shipped").sum("qty") > 10
```
{% if table.inbound %}

Pointing at this table: {% for i in table.inbound %}`{{ i.table }}Ↄ{{ i.field }}`{% if not loop.last %}, {% endif %}{% endfor %}.
{% elif table %}

No other table points at `{{ table.name }}`.
{% endif %}

#### The user

`user` is the logged-in user: `user.id`, `user.role` (1 is admin, 100 is public) and
every other field of the users table{% if user_fields %}:
{% for f in user_fields %}`user.{{ f.name }}`{% if not loop.last %}, {% endif %}{% endfor %}{% endif %}.
Nobody logged in means `user` is `null`, and a formula that reads `user.id` then grants
nothing.
{% if user %}

For you, now, `user` is:

```json
{{ user | tojson(indent=2) }}
```
{% endif %}

#### The operation

Five flags say what is being attempted, so one formula can grant different things:

| Flag | True when |
|---|---|
| `_read` | reading the row |
| `_insert` | inserting it (checked on the new row) |
| `_update` | updating it (checked on the row before **and** after the change) |
| `_delete` | deleting it |
| `_write` | any of insert, update and delete |

Checking an update on the row after the change too means a user cannot hand a row they
own to somebody else, unless the formula also grants them the new row.

## Examples

{% set ns2 = namespace(owner=none) %}
{% for f in table.fields if f.key_to and f.key_to.table == "users" %}{% if ns2.owner is none %}{% set ns2.owner = f.name %}{% endif %}{% endfor %}
{% if ns2.owner %}
Users own the rows that name them in `{{ ns2.owner }}`:

```js
{{ ns2.owner }} === user.id
```

Everybody can read; only the named user can change:

```js
_read || {{ ns2.owner }} === user.id
```
{% else %}
Users own the rows that name them in a key-to-users field `owner`:

```js
owner === user.id
```

Everybody can read; only the owner can change:

```js
_read || owner === user.id
```
{% endif %}

Users whose `department` (a field on the users table) is Finance own every row:

```js
user.department === "Finance"
```

The manager of the row's project owns it (through a key field `project`):

```js
projectⱵmanager === user.id
```

Users a document is shared with can read it, and write it if the share says so:

```js
sharesↃdocument.some(s => s.shared_with === user.id && (_read || s.can_write))
```

## Database row-level security

On Postgres, the formula can also be enforced by the database itself ("Enforce with
database row-level security"), so that even raw SQL run as the user is filtered. That
needs a formula that translates to SQL for every operation; saving tells you which part
does not.
