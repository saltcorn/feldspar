# Saltcorn Feldspar

This document outlines a high-level plan for the design of Saltcorn Feldspar (Saltcorn 2), the evolution of Saltcorn, an database application builder for web and mobile apps. The goals for the significant rewrite are outlined below after a statement of the scope

## Scope

A Rust library for a unified catalog and server in a cli command with web UI and APIs for 

  * relational and relation-like data sources. Can connect to multiple databases and also show other data as if it were a table in a database 
  * A workflow engine with durable workflows. Workflows are based on elementary actions. 
  * AI agents based on Traits (called "skills" in saltcorn v1, renamed traits to avoid confusion with agent skills), which are elementary agent capabilities that can be enabled and configures. E.g. ability to search on a table, an tool based on code
  * triggers: workflows and agents. 
  * file stores: connect multiple file stores 
  * predictive models. different types of models run 
  * users. unified authentication and authorization

Everything is focused around the relational data model. Workflows and actions can be run against rows in a database table.

Support for different ways of creating UIs for these entities: 
  
  * front end code based on React, NextJS, svelte etc. For each of these we provide the frontend api connector (react)
 or database interface (nextjs etc). The rust server process must serve the bundled assets. This is the initial focus.
  * A drag and drop building experience like saltcorn v1.

Backwards compatibility with saltcorn v1 is not a goal. Users will have to restore a backup (no in-place update v1 -> v2) and we will write specific import code for this.

## Goals compared to saltcorn 1

General:

- much more reliable, scalable and flexible than saltcorn v1. clean and simple code and types. Rewriting a core in clean Rust allows us to clean up and take into consideration everything we have learned and then expose a backwards-compatible interface in the JavaScript bindings
- polyglot. Everything can be written in many different languages. JavaScript, Rust, Python, Java, C#, Go. Plugin mechanisms need to be defined for each of those. Initial focus is on Javascript and Rust
- multiple applications for a single data layer. Each application can have access to a subset of tables, file stores etc.
- message bus which is backed by a message bus driver, with an option to use postgres LISTEN/NOTIFY for a simple bus backed by the exisitng database (and another option built in, using a rust library such as apalis or zeromq) and other drivers for Redis, Kafka etc for scalability. real time chat, real time collaboration, cache updates flow over the bus
- full restart should never be required. Only individual apis and applications may need restart
- Error handling: Log every error into an error log in the database. Distinguish between 
Application errors (errors in the configuration, for instance the person building an app has entered an invalid equation somewhere) and System errors (something crashed, there is likely an error in the Saltcorn code somewhere)
- streams - a new entity type representing dataflows
- internationalisation: one translation facility covering the core's own messages, the admin UI, and the applications built here. Three populations of string: ours (shipped translated), the admin's (translated per installation, with an LLM to do the work), and the end user's data (later)

Auth:

- access control lists/language
- can be an identity provider. Option to enable oauth2 server 
- authentication at the level of google auth: notice new devices, email if new device
- authorization backed by row level security if available in the database, or runtime checks
- set permissions separately for reading, creating, updating and deleting

Data:

- able to connect many different databases
- facilitate working with legacy databases. No need to discover databases, as soon as a database is connected its tables are usable
- can work with any database design, e.g. composite primary keys, foreign keys to non-primary key fields
- multiple file stores. some file stores are recognized as git repositories.
- lower level query language: enum-based representation of an sql query. inspired by seaquery but data (array of enum values) instead of fluent function calls.
- key types especially JSON are built in.
- do not create primary key fields when table is created. User must create primary key fields like other fields

Workflows and triggers:

- ensure that the workflow engine matches modern workflow engines for durability and error handling. Look at [DBOS](https://docs.dbos.dev/architecture), temporal ([1](https://medium.com/data-science-collective/system-design-series-a-step-by-step-breakdown-of-temporals-internal-architecture-52340cc36f30), [2](https://docs.temporal.io/evaluate/understanding-temporal)) and [restate](https://www.restate.dev/blog/building-a-modern-durable-execution-engine-from-first-principles)
- workflow execution code is a mess in saltcorn v1. This needs to be much cleaner.
- The number of built-in workflow actions should be minimal
- explicit guarantees about execution: Each step runs in a transaction. Each step is run at least once: if the workflow engine crashes during execution, it will resume  the step it was running. Error handling settings per workflow, acan be overridden per step: A designated error handling step or explicit retries up to a set limit with a configurable backoff policy. 

API:

- high-quality API for applications. Look at Hasura, postgrest, supabase api for this
- enable REST, GraphQL, gRPC, tRPC, and MCP APIs per-application
- each of these run by an API Provider
- the API is enabled as part of an application (see below)
- The API should offer access to tables and actions per permissions settings and also custom routes using user-written code (in a supported language) and SQL queries.
- all APIs should generate information and typescript type declarations (or a typed API-consumer library). This is the case both for the admin UI and APIs enabled per application. 
- in the graphQL api, aggregations are optional and enabled with a switch
- The rest API should allow selecting join fields in the same way done by postgREST: optional field list in queery string can have key_name(joinfield1, joinfield2)
- The REST api should include in its configuration custom SQL queries. these must have input parameters that are specified by types
- select HTTP method manually per custom SQL query
- There must be a button to update the client code in each code application following table changes. This should also check if rescaffolding is needed, e.g. if the application directory is empty. 
- If the api defintion changes, the client code must be updated automatically.
- the generated typescript client subdirectory must have a README.md file that explains that this directory is maintained externally and any changes will be overwritten
- the feldspar cli must include a command to add a custom SQL query to the API. This must update the  
- the generated typescript client directory must include the SQL definition for the connected tables, to help any coding agents running in the directory formulate custom queries.
- when scaffolding typescript apps, include an AGENTS.md file that points to the README.md file for the client and explains how to add a custom SQL query with the cli. 

Admin UI:

- only admin users can log in. Later, admins can give restricted access (e.g. only developing application) to some non-admin users
- separate the admin ui for the user serving routes. Admin ui has its own URL (could be a different subdomain, or a url path). But it is the same process
- enable strict content security policy
- much better file manager.
- builder is not built in to the admin ui. This comes with the saltcorn1 views. but the builder 
- much improved table editor. bring in as much functionality from airtable's admin ui as possible. 
- we will need to recreate a dynamic form framework. Again the client js for this in saltcorn got messy as it grew. Form framework needs to cover conditional fields (field shown depending on value of other fields), repeated forms (like orderlines on an order), selects where options are populated dynamically (from the server, or from client code) depending on other form values, dynamic attributes or contents depending on other form values, form validation
- This needs to be implemented in react due to high availability of underlying libraries i.e. craft and react-flow. There are also components for file managers.
- use Bootstrap 5.3 as the CSS framework with React. (react-bootstrap)
- Use TypeScript for the React code. Interactions with the Admin UI API must go through a typed Typescript library consumer.
- Use the tabler theme (tabler.io) for the admin UI.

Agents and Copilot:

- builtin copilot (chat) and appconstructor
- generate a SKILL.md file if they want to use an external coding agent.

Application UI:

- There are multiple application providers (Frameworks). Each application has one primary framework. 
- applications can be written with modern code front end frameworks like next.js and sveltekit and crossplatform mobile frameworks like react native. Code front ends live in a git repository that is equal to or a subdir in a selected file store, and can be edited in a in-browser editor - ideally VS Code for the web
- applications can also be built in the saltcorn1 experience which will continue to improve
- MAYBE? it should be possible to mix salcorn1 views/pages with code pages. not sure. its tempting to say that each application should be built with either some framework or salcorn1 views. Perhaps the right thing is that each application has a primary UI handler but can also bring in others.
- each application can contain any number of APIs, each served on a sub path
- Each application is served on a specific subdomain
- enable strict CSP

## Code entities (supplied by core or plugins)

Database driver: is instantiated once for connecting to a specific database. must execute queries and database operations and manage tables.

Database driver must be written in Rust. The remaining code entities can be written in any supported language.

Table provider: can provide a virtual table. It will look to the user as if it is a database table with fields and rows. Examples: SQL query, RSS feed, IMAP, instant messaging search. This has to interpret the universal query language. and return the rows corresponding to the query. Any provided table can optionally be materialised into a real table with options for syncing.

Types: Rich types: types known to saltcorn feldspar, with attributes and fieldviews. Basic types: other types not known. The database driver makes a correspondence between types in its database and rich types. 

Fieldviews: can that can display and possibly edit data types in HTML. Each fieldview can display/edit multiple (at least one) types. some catch-all fieldviews can edit any type. 

Actions: an elementary step in a workflow, or can be run alone. Has configuration, as output can write to context

Code adapters: a central facility for code entities to use Javascript or Python. These need to maintain an open interpreter that can be used to execute code. Within that interpreter, the entitites in the catalog need to be available. For javascript, this needs to be compatible with saltcorn v1. the code adapters are initialised as needed, no all instalations need all of them. Code in the guest language can provide any of the other code entitty types (except databse driver). 

Importers and exporters: move table data to/from different formats. 

Model providers: can be configured and applied to selected tables or datasets. Configuration consists of: 1. the dataset, i.e. selecting table fields and derived fields such as calculations, joinfields and aggregations, and any inclusion/exclusion criteria on the rows in the dataset 2. the main parameters that specify the model type and structure and 3. any hyperparameters that may need to be optimised by picking an outcome and spliting that data into training, test and validation sections. When inference is run on live data it provides a predictive model, i.e. an outcome can be predicted for a row. In some models we are more interested in the parameter values, for instance in a regression model we may be more interested in the slope coefficient values. Models can be used for supervised learning (a field is chosen in the dataset as the label, float point or integer for regression or boolean, string with otions or integer for classification; prediction per row is label and uncertainty information), unsupervised (there is no label, but each row has a prediction; cluster number for clustering models, vector for dimensionality reduction models like PCA), statistical hypothesis testing (t-test, anova etc; the main outcome is the model parameters including the test statistic score) and bayesian inference (e.g. using https://mc-stan.org/ - model configuration is the model code in a stan file where the data section needs to be linked to the dataset). The core rust code supplies model providers for regression, clssification, clustering and dimensionality reduction based on smartcore. this needs to be a cargo feature flag so it can be disabled. bundled python plugins supply models based on scikit-learn and statsmodel.

there are more for saltcorn1 views

Viewpatterns: as in saltcorn v1.

Stream providers: can provide a stream, when the configuration fields are filled in. The stream provider desclares
 its configuration fields, and then as a function of these configuration fields can declare the stream element type (e.g. JSON, with known keys each having a known part; text (whcih encoding?) or binary) and some way of observing the elements of the stream. Stream providers are supplied by plugin modules, and there is a built-in stream provider for MQTT.

## Created Entities (created by applciation developer)

Cache: all entities except users, workflow runs and files are cached in memory for performance. When a transaction has modified an entity it should signal to all other connected entities to reload the cache for the changed entities. 

all of the following can be seen and edited in the admin web UI

Fields: cleaner than saltcorn 1 (we confused DB fields and Form fields). There should be interfaces for BaseField (shared properties), DataField (a field in a database table) and FormField (a form field) with options to convert between them. Fields can be calculated, either stored or not stored. These are defined either by simple expressions that can access related fields in either directions of foreign keys, or by running code in one of the code adapters. dependencies between calculated fields need to be carefully considered. If no calculated fields are using code adapters, everything can be implmeneted as triggers with a recursion limit. But if simple expressions are mixed with e.g. javascript code that runs custom functions, their dependencies and dependencies on that field needs to sorted topologically. Key fields and File fields are special. Key fields hold the value of the referenced field (not necessarily the primary key) and can as its attributes also have a "summary field" selection, which is another field on the target table that by default can be used as the label when selecting. File fields are defined by the relative file path in the target file store. The file store name is a attribute setting as well as restriction on the file type and location (may be restricted to a specific folder). 

Table: Every table had a table provider (may be a database driver). Each table has an array of fields, any number of rows, and settings for authorization: in saltcorn 1 we had roles (each user has a role), ownership fields and ownership formulae. 

Users: each user has a role, which is an integer 1-100. 1 is admin with full access. 100 is public (not logged in). USers can also have additional fields created by the admin.

Workflows: similar to v1. Every workflow is a trigger. Each workflow consists of a number of steps, each of which can read and write to a context for that run.

Workflow runs: similar to v1. Each run has a context, and optionally can also be traced so the context is stored after each step.

Agents: similar to v1. An agent is a type of action., 

Triggers: triggers can be actions, workflows or agents. A trigger is defined by: name, when (event that triggers it)

File stores: Connect any directory or other sources (S3) as a file store to the catalog. Each file store has a unique name

Files: files can have access rules set. also per directory. To access a file, the user needs the right to access every directory in its path.

Datasets: datasets is an internal entity used by fitting predictive models to table data. A dataset is defined by selecting a table first, then selecting table fields and derived fields such as calculations, joinfields and aggregations, and any inclusion/exclusion criteria on the rows in the dataset. Datasets do not have their own tab in the admin UI

Predictive models: There are different model providers. E.g. scikit learn model, mc-stan model etc. Each model is run from a model provider with specified configuration. Then a model can be run against a subset of the data (also by setting hyperparameters; the model provider defines what hyperparameters it has). Running a model creates a model instance. This has parameters that can be inspected, which may be the main point of the fit. Or it can be applied to a new row in a table. The model provider defines what the outcome is, depending on the configuration parameters.

Tags can be created in the admin UI any created entity gave have a tag applied to it. This helps when selecting several entities, a tag can be selected instead so the operation is applied to each entity in the tag

Applications can be created in the admin UI. When creating an application, the admin picks the Framework. Different frameworks have different settings, for instance a React application will need a file store or a subdirectory in a filestore to be the react code. The application also is configured with the subdomain on which it is served, any number of APIs that are created under an application, and any number of subdirectories that are served statically.

Streams: created from stream providers. Can be observed in the admin UI, by applications through their API (e.g. with an authenticated websocket interface the application can connect to), or can become the triggering event of a trigger. In the Data Layer part of the admin UI, there is a Streams link, linking to a list of the created streams, each with links to Editing, Observing and Deleting. At the top is a link to create a new stream.

Translations: each application declares which locales it offers. The strings an admin wrote when building the application - labels in a code application's source, text in a builder layout - are collected and listed per locale in the admin UI, where they can be typed in or filled in by an LLM. The core's own strings and the admin UI's are translated by us and shipped. A user has a language, and everything they are shown is in it: an admin sees the admin UI in their own language, and an application's visitor sees the application in theirs.

## Authorization

We will use a similar authorization for table data as in Saltcorn1, but the notion of ownership fields will now be folded into ownership formulae which are more general. Each table may have an ownership formula which is a JavaScript expression in which the following is in scope:

* each field name in the current table, which has the value of the field in the current row
* the `user` variable which is an object with all user-defined fields of the current user. If no user is logged in, `user` is null 
* the boolean variables `_read` which is true if and only if the access operation is a select, `_update`, `_insert` and `_delete` for update, insert and delete operations and the general `_write` if the operation is an update, insert or delete.
* any joinfields on the current table can be used to access a value on the target field using the Claudian half-h (Ⱶ) operator. for instance if we have a table called books which has a publisher field which is a foreign key on to a publisher table which has a name field, then publisherⱵname can be used to access name on the publisher table. The half-h works as optinal chaining, so if the foreign key is null the whole expression is null. Half-h operators can be chained to any depth.

This is optionally translated to row-level security policies with postgres. The table has a rls_enabled field in its json attributes in the metadata, RLS can be enabled in the admin UI. In that case the javascript formula must be translated to an RLS policy and then you no longer have to check. This helps especially as joinfields no longer have to be refetched.

If there are any stored calculated fields, then in the RLS policy translation these will be used as their calculation definitions, not as their sotred values during RLS policy evaluation.

## Code guidelines

Principles:

1. simple and clean
2. minimise the number of lines of code in the project. Every line of code is a liability
3. Use abstractions, but avoid making them overly complicated. 
4. Testing. Everything must be covered by integration tests. Where possible also by unit tests. But do not complicate the design in order to increase testability.
5. no silent failures. make it crash and display error message unless the error can be handled.
6. monorepo. All code except plugins is in one mono repo
7. Separation of concerns. try to split out functionality in to generic libraries that are separate crates. 

### build vs introduce dependency

### code reuse

### Target Platforms

We are targeting Linux, MacOS, Windows and FreeBSD 

### On-disk metadata storage

We are not using the same storage format as saltcorn v1, but it is similar

All metadata and users are stored in the primary database. Any table in the primary table called `_fd_*` is regarded as a system system metadata table and not visible to the user. Any system metadata table must have: name, id (uuid), description, attributes (JSON field, always an object) and any other fields. Fields that has a value for many rows should be their own field, fields that have a sparse value can be set in the attributes. This is a value judgement and key part of the design.

Files are on-disk, there is no database reppresentation per file. any per-file metadata must be stored as xattrs, we need a cross platform library to access this

Tables and fields : all tables and fields work out of the box when a database driver is connected. So no metadata is strictly necessary for the tables. But both tables and fields may need to have metadata added to them - access rules, attributes. The primary database contains a table for metadata called `_fd_tables` and `_fd_fields` that stores an "overlay" on the existing tables with any additional information and also details on any provided tables. 

Triggers, agents and workflows: Stored in the `_fd_triggers` table. Workflows must be versioned so a suspended run can finish with its version of the workflow.

Workflow and agent runs: stored in the `_fd_runs` table which stored the current context and state (updated after each step). When enabled for a specific workflow, runs can write the centext and timing of each step to the `_fd_run_traces` table. 

Configuration: stored in a `_fd_config` table. Configurations can apply to the setup as a whole or each application, and specific frameworks will have different configuration values. for each permissible key, there must be a restriction on the types of values this can take, but all values are stored as JSON values.

Applications: applications are stored in a `_fd_applications` table. 

Models and model instances: stored in `_fd_models` and `_fd_model_instances` tables

Users: users are stored in a database table called `users` in the primary database. Passwords are stored encrypted according to best practices. The user primary key should be UUID, for importing legacy saltcorn applications where the user id was autoincrementing integers, a legacy_id field can be created as needed. Initially every user has an email, but this field can be deleted by the admin and a different field can be introduced. Code should never assume the user has any other field than the id (which must not be deletable). Admin can add any field to the user table.

Migrations: Similar to Saltcorn1: an ordered list of migrations, each run once on the first start of a release that adds it and recorded so that it never runs again. A migration is SQL (one file per database backend) or a Rust function, registered in Rust. Additive changes (new tables, new nullable columns) are still made by the bootstrap; a migration is for what the bootstrap cannot do. See TECHNICAL_DESIGN §5.3.

### HTTP server

- must be dynamic enough to add routes that are not known at compile time. So not everything can be statically typed. 
- must be able to generate TypeScript typed API consumer libraries 
- The Admin UI API is known at compile time. But in order to maximise code reusability make it fixed-values for the dynamic API route definition and use the same machinery to generate a typescript API consumer.
- So we need a representation of API endpoints as a rust value, with a resentation of the types for arguments and result values

- Server must send a systemd notification when ready on linux, but must still compile and skip this step on non-linux or on linux without systemd. Ideally do not depend on libsystemd-dev being installed, if possible
- certificates must be obtained by an ACME server like LetsEncrypt, or allow admin user to enter certificates manually.

## Milestones

### MVP

Scope: Database drivers, tables, fields, users. admin UI for tables, fields and users. Single database only (same as the primary data store).

Library: 

Query is an enum whcih allows us to build "select fields... from table_name where ... limit ...". Also inserts, updates, deletes

Postgresql Database driver is an object created with postgres host, username, password, db name etc. Methods are to run query etc

Catalog is an object. The catalog (cache of tables and fields) is initialised with a database driver. it uses information schema to find tables and fields. Cache has methods for getting, creating and creating table and field. There is no stored metadata outside the information_schema

Types: no Rich types. all types are basic types

server routes: the server routes for the web admin UI live in a crate for the server.

Tests: the tests must be run against a real postgres database, which is reinitialised at the bginning of every test. test table creation, field creation, initialising the catalog with exisitng tables

CLI that can run server for admin UI

The user can: when there is no user, login directs to "create first user" screen; create table, show list of fields in table, create fields, edit rows, create users. Users can login and log out

Files: a file store can be connected. Basic file manager and ability to edit files

an app can be built on react, completely served from Saltcorn process. the app has no connection to database. The app lives in a file store which is a git repository. there must be a build step 

api to serve the react app. authentication from react app.

this is MVP - the system is now useful.

## UNRESOLVED

- can you mix react and saltcorn1 applications?
- how are we creating emails. 
- auth features beyond device recognition
