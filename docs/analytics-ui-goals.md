# Analytics UI goals

This document contains the goals for a new analytics interface and application framework for feldspar, the Analytics UI, along with some changes to the core framework.

The scope for the framework is predictive analytics, including:

* the creating and editing of datasets, which are derived from database tables
* exploratory data analysis
* dashboards for non-technical users 
* fitting statistical models
* hypothesis testing* notebook interfaces
* GIS work: maps with layers, spatiotemporal data analysis
* reports

The goal here is not to be the most powerful data analytics package but to provide the 50% of features that are sufficient for 95% of users, ideally with a plug-in architecture so that entity types can provide expert functionality. Prioritise ease of use over feature completeness.

The unrestricted version of the analytics UI appears as the Analytics link in the admin UI sidebar menu (replacing the "Predictive models" link). This leads to a version of the analytics UI where all tables and workspaces are accessible. The admin can also create an application using the Analytics UI framework, which is a restricted subset of the Analytics UI functionality. It might be only a particular workspace (e.g. dashboard or report), or it might give the end user more power with a self-serve analytics environment but that has access only to a restricted subset of tables.

### Workspaces

The full analytics UI consists of a number of workspaces each of which takes a specific form. When entering the analytics UI, the user see the list of existing workspace and can enter one, or create a new one by name and type.
Workspace type:
* Dataset editor - initial screen is a list of datasets, each has a link to edit, clone and delete, or create new. Each dataset is based on a base table which is picked when creating new but cannot be changed. In the individual dataset editor is a spreadsheet like read only view of data. New columns can be added with a plus in the last column header. The persisted list of datasets is global, the dataset editor will open one of them (unless it is in the initial state of looking at the list of global datasets before picking one to edit)
* Data explorer: interactively creating different visualizations and summary tables without any persistence other than opening up in the same state where it was left off last time. A single screen where the data set is chosen in a drop-down and then the plot / summary table type. Then the parameters and configuration for each plot type is chosen and the plot is shown. The data explorer can also perform simple hypothesis tests, which sit alongside a plot type. Large models like general linear models are done through the model fit interface.
* Dashboard: a tiled view including multiple plots and summary table and summary statistics cards, it may be interactive to enable drill down / cross filtering. There is no base data set for a dashboard; it can freely combine plots and summary tables across multiple datasets.
* Model fit: a workspace for creating and editing model fits to datasets. Starts with a list of existing models each of which can be edited, cloned or deleted or the user can create a new model. Each model has a dataset and the model provider. There is an interface for editing the model parameters, fitting to a model, seeing fit progress, and then the fit output for that model below when done. Like the dataset editor, the list of models is global and the model fit it tied to one of them, unless it is in the initial state of picking a model to edit.
* Notebook: the notebook is a jupyter- style notebook that contains code blocks, text blocks and output blocks. The code blocks are in a language that is set at creation time; it can be either JavaScript, Python or it can be natural language prompts to an LLM. The functions available allow it to generate panels, fit models or create non-persisted datasets.
* Report: similar to a dashboard but intended to generate printable PDFs. Not interactive for drill down statistics.
* Map: a map for GIS work. Has a base map and layers of data that can be added. The data for these layers comes from datasets.

Initially only one workspace is open at a time, however the display can be split side by side to have two open workspaces. 

### Panels

One thing that is bringing these workspace types together is a unifying notion of panels. Panel is an elementary output, most importantly a plot, but also summary tables. Panels can then be dragged between workspaces. Drag and drop is always copy it never deletes a panel in the source

Some rules for drag and drop of panels

Sources:

* The current output of the data explorer is a draggable panel
* The model fit producers and number of draggable panels 
* The notebook output may be a panel that can be dragged. 
* Reports or dashboards are also sources
* Any map as a whole is a single panel that can be dragged.

Sinks:
* Anything can be dragged into a report or dashboard

### Additional changes to core
* Dataset definitions now become persistent, but their rows are not materialised. There is a table to represent dataset definitions.
* The predictive models menu link is replaced by a link to the unrestricted analytics UI
* Models fits have outputs: tables and plots. Some plots may be optional, i.e. not initially shown but available in a drop-down.
* the definition of models and models providers is still open and should be tweaked to align with the goals in this specification

There is no coding agent for this application type at this point. 

### Milestones

1. Workspace persistence+UI, Dataset editor workspace
2. Add Data explorer workspace which defines the available plot types. No drag and drop
3. Model fit workspace with output panels. No drag and drop.
4. Reports and enabling drag and drop of panels from the data explorer.
5. Maps.
6. Dashboards
7. Application framework for restricted analytics UIs. Everything before this milestone is the unrestricted analytics UI accessed by the admin through the site bar link.

Open questions:

- is this the right set of workspace types
- in the data explorer can we bring in ideas from the grammar of graphics in an interactive interface?
- gis maps: having maps with special data that can be overlaid in layers seems straightforward. What is less obvious to me is how that interacts with models and how model outputs for spatial temporal models can also be overlaid on maps. We need a better handle on the types of transformations of spatial data