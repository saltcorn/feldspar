//! Every built-in handler the server dispatches to, with who may call it
//! (security review 2026-11 §8, E4; TODO-type-hardening H6.1).
//!
//! One [`HandlerId<A>`] per handler. The endpoint names it
//! ([`Endpoint::new`](crate::Endpoint::new) takes its requirement from `A`, with
//! no default and no setter) and `sc-server` registers the handler under the
//! same id (`HandlerRegistry::handle`), which hands the handler what `A`
//! admits — an admin's authority for [`AdminOnly`], a signed-in user for
//! [`SignedIn`], the Analytics UI's caller for [`AnalyticsUi`], anybody for
//! [`Anyone`]. Before, the two were joined by a string and a startup check
//! that only looked at `admin()` and `LoggedIn`; now a declaration and its
//! handler cannot disagree about who may call it, and this file is the one
//! list to read to see every handler's authority.
//!
//! The names are the operation names the generated TypeScript clients call,
//! unchanged: nothing on the wire moves.

use crate::endpoint::{AdminOnly, AnalyticsUi, Anyone, HandlerId, SignedIn};

// --- `admin::agent_memory` ---

pub const LIST_AGENT_MEMORIES: HandlerId<AdminOnly> = HandlerId::new("listAgentMemories");
pub const DELETE_AGENT_MEMORY: HandlerId<AdminOnly> = HandlerId::new("deleteAgentMemory");

// --- `admin::agents` ---

pub const LIST_AGENTS: HandlerId<AdminOnly> = HandlerId::new("listAgents");
pub const CREATE_AGENT: HandlerId<AdminOnly> = HandlerId::new("createAgent");
pub const UPDATE_AGENT: HandlerId<AdminOnly> = HandlerId::new("updateAgent");
pub const DELETE_AGENT: HandlerId<AdminOnly> = HandlerId::new("deleteAgent");
pub const LIST_AGENT_TRAITS: HandlerId<AdminOnly> = HandlerId::new("listAgentTraits");
pub const LIST_RUNS: HandlerId<AdminOnly> = HandlerId::new("listRuns");
pub const GET_RUN: HandlerId<AdminOnly> = HandlerId::new("getRun");
pub const GET_RUN_PART: HandlerId<AdminOnly> = HandlerId::new("getRunPart");
pub const GET_RUN_DIFF: HandlerId<AdminOnly> = HandlerId::new("getRunDiff");
pub const DELETE_RUN: HandlerId<AdminOnly> = HandlerId::new("deleteRun");

// --- `admin::applications` ---

pub const LIST_APPLICATIONS: HandlerId<AdminOnly> = HandlerId::new("listApplications");
pub const CREATE_APPLICATION: HandlerId<AdminOnly> = HandlerId::new("createApplication");
pub const UPDATE_APPLICATION: HandlerId<AdminOnly> = HandlerId::new("updateApplication");
pub const DELETE_APPLICATION: HandlerId<AdminOnly> = HandlerId::new("deleteApplication");
pub const BUILD_APPLICATION: HandlerId<AdminOnly> = HandlerId::new("buildApplication");
pub const DEEP_CLEAN_APPLICATION: HandlerId<AdminOnly> = HandlerId::new("deepCleanApplication");
pub const BUILD_APPLICATION_TARGET: HandlerId<AdminOnly> = HandlerId::new("buildApplicationTarget");
pub const RUN_APPLICATION_TARGET_OPERATION: HandlerId<AdminOnly> =
    HandlerId::new("runApplicationTargetOperation");
pub const GET_APPLICATION_TARGET_BUILD: HandlerId<AdminOnly> =
    HandlerId::new("getApplicationTargetBuild");
pub const UPDATE_APPLICATION_CLIENT: HandlerId<AdminOnly> =
    HandlerId::new("updateApplicationClient");
pub const RUN_APPLICATION_GRAPHQL: HandlerId<AdminOnly> = HandlerId::new("runApplicationGraphql");
pub const LIST_FRAMEWORKS: HandlerId<AdminOnly> = HandlerId::new("listFrameworks");
pub const LIST_API_PROVIDERS: HandlerId<AdminOnly> = HandlerId::new("listApiProviders");
pub const DESCRIBE_CUSTOM_QUERY: HandlerId<AdminOnly> = HandlerId::new("describeCustomQuery");
pub const LIST_BUILD_HOSTS: HandlerId<AdminOnly> = HandlerId::new("listBuildHosts");
pub const CREATE_BUILD_HOST: HandlerId<AdminOnly> = HandlerId::new("createBuildHost");
pub const UPDATE_BUILD_HOST: HandlerId<AdminOnly> = HandlerId::new("updateBuildHost");
pub const DELETE_BUILD_HOST: HandlerId<AdminOnly> = HandlerId::new("deleteBuildHost");

// --- `admin::auth` ---

pub const AUTH_STATUS: HandlerId<Anyone> = HandlerId::new("authStatus");
pub const FINISH_ONBOARDING: HandlerId<AdminOnly> = HandlerId::new("finishOnboarding");
pub const CREATE_FIRST_USER: HandlerId<Anyone> = HandlerId::new("createFirstUser");
pub const LOGIN: HandlerId<Anyone> = HandlerId::new("login");
pub const LOGOUT: HandlerId<SignedIn> = HandlerId::new("logout");

// --- `admin::backup` ---

pub const GET_BACKUP_OPTIONS: HandlerId<AdminOnly> = HandlerId::new("getBackupOptions");
pub const UNLOCK_BACKUP: HandlerId<AdminOnly> = HandlerId::new("unlockBackup");
pub const RESTORE_BACKUP: HandlerId<AdminOnly> = HandlerId::new("restoreBackup");
pub const LIST_BACKUP_SCHEDULES: HandlerId<AdminOnly> = HandlerId::new("listBackupSchedules");
pub const CREATE_BACKUP_SCHEDULE: HandlerId<AdminOnly> = HandlerId::new("createBackupSchedule");
pub const UPDATE_BACKUP_SCHEDULE: HandlerId<AdminOnly> = HandlerId::new("updateBackupSchedule");
pub const DELETE_BACKUP_SCHEDULE: HandlerId<AdminOnly> = HandlerId::new("deleteBackupSchedule");
pub const GET_CLEAR_ALL_PREVIEW: HandlerId<AdminOnly> = HandlerId::new("getClearAllPreview");
pub const CLEAR_ALL: HandlerId<AdminOnly> = HandlerId::new("clearAll");

// --- `admin::checklists` ---

pub const LIST_CHECKLISTS: HandlerId<AdminOnly> = HandlerId::new("listChecklists");
pub const CREATE_CHECKLIST: HandlerId<AdminOnly> = HandlerId::new("createChecklist");
pub const GET_CHECKLIST: HandlerId<AdminOnly> = HandlerId::new("getChecklist");
pub const UPDATE_CHECKLIST: HandlerId<AdminOnly> = HandlerId::new("updateChecklist");
pub const DELETE_CHECKLIST: HandlerId<AdminOnly> = HandlerId::new("deleteChecklist");
pub const ADD_CHECKLIST_STEP: HandlerId<AdminOnly> = HandlerId::new("addChecklistStep");
pub const REORDER_CHECKLIST_STEPS: HandlerId<AdminOnly> = HandlerId::new("reorderChecklistSteps");
pub const RUN_CHECKLIST: HandlerId<AdminOnly> = HandlerId::new("runChecklist");
pub const LIST_CHECKLIST_RUNS: HandlerId<AdminOnly> = HandlerId::new("listChecklistRuns");
pub const RUN_CHECKLISTS: HandlerId<AdminOnly> = HandlerId::new("runChecklists");
pub const GET_CHECKLIST_RUN: HandlerId<AdminOnly> = HandlerId::new("getChecklistRun");
pub const GET_CHECKLIST_SCREENSHOT: HandlerId<AdminOnly> = HandlerId::new("getChecklistScreenshot");
pub const UPDATE_CHECKLIST_STEP: HandlerId<AdminOnly> = HandlerId::new("updateChecklistStep");
pub const DELETE_CHECKLIST_STEP: HandlerId<AdminOnly> = HandlerId::new("deleteChecklistStep");
pub const ACCEPT_CHECKLIST_STEP: HandlerId<AdminOnly> = HandlerId::new("acceptChecklistStep");
pub const ACCEPT_CHECKLIST_PROPOSAL: HandlerId<AdminOnly> =
    HandlerId::new("acceptChecklistProposal");
pub const REJECT_CHECKLIST_PROPOSAL: HandlerId<AdminOnly> =
    HandlerId::new("rejectChecklistProposal");
pub const LIST_CHECKLIST_STEP_TYPES: HandlerId<AdminOnly> =
    HandlerId::new("listChecklistStepTypes");
pub const LIST_CHECKLIST_STARTERS: HandlerId<AdminOnly> = HandlerId::new("listChecklistStarters");
pub const CREATE_CHECKLIST_STARTER: HandlerId<AdminOnly> = HandlerId::new("createChecklistStarter");
pub const SET_CHECK_USER: HandlerId<AdminOnly> = HandlerId::new("setCheckUser");
pub const LIST_CHECKLIST_USERS: HandlerId<AdminOnly> = HandlerId::new("listChecklistUsers");

// --- `admin::code_libraries` ---

pub const LIST_CODE_LIBRARIES: HandlerId<AdminOnly> = HandlerId::new("listCodeLibraries");
pub const CREATE_CODE_LIBRARY: HandlerId<AdminOnly> = HandlerId::new("createCodeLibrary");
pub const UPDATE_CODE_LIBRARY: HandlerId<AdminOnly> = HandlerId::new("updateCodeLibrary");
pub const DELETE_CODE_LIBRARY: HandlerId<AdminOnly> = HandlerId::new("deleteCodeLibrary");

// --- `admin::connections` ---

pub const LIST_DATABASE_CONNECTIONS: HandlerId<AdminOnly> =
    HandlerId::new("listDatabaseConnections");
pub const CREATE_DATABASE_CONNECTION: HandlerId<AdminOnly> =
    HandlerId::new("createDatabaseConnection");
pub const UPDATE_DATABASE_CONNECTION: HandlerId<AdminOnly> =
    HandlerId::new("updateDatabaseConnection");
pub const DELETE_DATABASE_CONNECTION: HandlerId<AdminOnly> =
    HandlerId::new("deleteDatabaseConnection");
pub const TEST_DATABASE_CONNECTION: HandlerId<AdminOnly> = HandlerId::new("testDatabaseConnection");

// --- `admin::entities` ---

pub const LIST_ENTITIES: HandlerId<AdminOnly> = HandlerId::new("listEntities");

// --- `admin::fields` ---

pub const LIST_FIELDS: HandlerId<AdminOnly> = HandlerId::new("listFields");
pub const LIST_INBOUND_KEYS: HandlerId<AdminOnly> = HandlerId::new("listInboundKeys");
pub const CREATE_FIELD: HandlerId<AdminOnly> = HandlerId::new("createField");
pub const UPDATE_FIELD: HandlerId<AdminOnly> = HandlerId::new("updateField");
pub const DELETE_FIELD: HandlerId<AdminOnly> = HandlerId::new("deleteField");
pub const LIST_CONSTRAINTS: HandlerId<AdminOnly> = HandlerId::new("listConstraints");
pub const CREATE_CONSTRAINT: HandlerId<AdminOnly> = HandlerId::new("createConstraint");
pub const DELETE_CONSTRAINT: HandlerId<AdminOnly> = HandlerId::new("deleteConstraint");
pub const LIST_FIELD_TYPES: HandlerId<AdminOnly> = HandlerId::new("listFieldTypes");

// --- `admin::file_stores` ---

pub const LIST_FILE_STORES: HandlerId<AdminOnly> = HandlerId::new("listFileStores");
pub const CREATE_FILE_STORE: HandlerId<AdminOnly> = HandlerId::new("createFileStore");
pub const UPDATE_FILE_STORE: HandlerId<AdminOnly> = HandlerId::new("updateFileStore");
pub const DELETE_FILE_STORE: HandlerId<AdminOnly> = HandlerId::new("deleteFileStore");
pub const LIST_FILE_STORE_BACKENDS: HandlerId<AdminOnly> = HandlerId::new("listFileStoreBackends");
pub const RUN_BACKEND_OPERATION: HandlerId<AdminOnly> = HandlerId::new("runBackendOperation");
pub const RUN_FILE_STORE_OPERATION: HandlerId<AdminOnly> = HandlerId::new("runFileStoreOperation");

// --- `admin::files` ---

pub const BROWSE_FILES: HandlerId<AdminOnly> = HandlerId::new("browseFiles");
pub const FIND_FILES: HandlerId<AdminOnly> = HandlerId::new("findFiles");
pub const LIST_STORE_FILES: HandlerId<AdminOnly> = HandlerId::new("listStoreFiles");
pub const SEARCH_FILES: HandlerId<AdminOnly> = HandlerId::new("searchFiles");
pub const READ_FILE: HandlerId<AdminOnly> = HandlerId::new("readFile");
pub const WRITE_FILE: HandlerId<AdminOnly> = HandlerId::new("writeFile");
pub const MAKE_DIRECTORY: HandlerId<AdminOnly> = HandlerId::new("makeDirectory");
pub const DELETE_FILE: HandlerId<AdminOnly> = HandlerId::new("deleteFile");
pub const RENAME_FILE: HandlerId<AdminOnly> = HandlerId::new("renameFile");
pub const GET_FILE_META: HandlerId<AdminOnly> = HandlerId::new("getFileMeta");
pub const SET_FILE_META: HandlerId<AdminOnly> = HandlerId::new("setFileMeta");

// --- `admin::llm` ---

pub const LIST_LLM_PROVIDERS: HandlerId<AdminOnly> = HandlerId::new("listLlmProviders");
pub const CREATE_LLM_PROVIDER: HandlerId<AdminOnly> = HandlerId::new("createLlmProvider");
pub const UPDATE_LLM_PROVIDER: HandlerId<AdminOnly> = HandlerId::new("updateLlmProvider");
pub const DELETE_LLM_PROVIDER: HandlerId<AdminOnly> = HandlerId::new("deleteLlmProvider");
pub const SET_DEFAULT_LLM_PROVIDER: HandlerId<AdminOnly> = HandlerId::new("setDefaultLlmProvider");
pub const LIST_LLM_PROVIDER_BACKENDS: HandlerId<AdminOnly> =
    HandlerId::new("listLlmProviderBackends");
pub const LIST_LLM_MODELS: HandlerId<AdminOnly> = HandlerId::new("listLlmModels");
pub const CREATE_LLM_MODEL: HandlerId<AdminOnly> = HandlerId::new("createLlmModel");
pub const UPDATE_LLM_MODEL: HandlerId<AdminOnly> = HandlerId::new("updateLlmModel");
pub const DELETE_LLM_MODEL: HandlerId<AdminOnly> = HandlerId::new("deleteLlmModel");
pub const LIST_LLM_MODEL_SETTINGS: HandlerId<AdminOnly> = HandlerId::new("listLlmModelSettings");
pub const FETCH_LLM_MODELS: HandlerId<AdminOnly> = HandlerId::new("fetchLlmModels");
pub const TEST_LLM_MODEL: HandlerId<AdminOnly> = HandlerId::new("testLlmModel");

// --- `admin::mcp_servers` ---

pub const LIST_MCP_SERVERS: HandlerId<AdminOnly> = HandlerId::new("listMcpServers");
pub const CREATE_MCP_SERVER: HandlerId<AdminOnly> = HandlerId::new("createMcpServer");
pub const UPDATE_MCP_SERVER: HandlerId<AdminOnly> = HandlerId::new("updateMcpServer");
pub const DELETE_MCP_SERVER: HandlerId<AdminOnly> = HandlerId::new("deleteMcpServer");
pub const TEST_MCP_SERVER: HandlerId<AdminOnly> = HandlerId::new("testMcpServer");
pub const LIST_MCP_SERVER_TOOLS: HandlerId<AdminOnly> = HandlerId::new("listMcpServerTools");

// --- `admin::models` ---

pub const LIST_MODEL_PROVIDERS: HandlerId<AdminOnly> = HandlerId::new("listModelProviders");
pub const PREVIEW_DATASET: HandlerId<AdminOnly> = HandlerId::new("previewDataset");
pub const LIST_MODELS: HandlerId<AdminOnly> = HandlerId::new("listModels");
pub const GET_MODEL: HandlerId<AdminOnly> = HandlerId::new("getModel");
pub const SAVE_MODEL: HandlerId<AdminOnly> = HandlerId::new("saveModel");
pub const CLONE_MODEL: HandlerId<AdminOnly> = HandlerId::new("cloneModel");
pub const MODEL_USAGE: HandlerId<AdminOnly> = HandlerId::new("modelUsage");
pub const PATCH_MODEL_VIEW_STATE: HandlerId<AdminOnly> = HandlerId::new("patchModelViewState");
pub const DELETE_MODEL: HandlerId<AdminOnly> = HandlerId::new("deleteModel");
pub const FIT_MODEL: HandlerId<AdminOnly> = HandlerId::new("fitModel");
pub const LIST_MODEL_INSTANCES: HandlerId<AdminOnly> = HandlerId::new("listModelInstances");
pub const GET_MODEL_INSTANCE: HandlerId<AdminOnly> = HandlerId::new("getModelInstance");
pub const DELETE_MODEL_INSTANCE: HandlerId<AdminOnly> = HandlerId::new("deleteModelInstance");
pub const ACTIVATE_MODEL_INSTANCE: HandlerId<AdminOnly> = HandlerId::new("activateModelInstance");
pub const PREDICT_ROWS: HandlerId<AdminOnly> = HandlerId::new("predictRows");
pub const GET_PROGRAM_INTERFACE: HandlerId<AdminOnly> = HandlerId::new("getProgramInterface");
pub const PREVIEW_MODEL_DATA: HandlerId<AdminOnly> = HandlerId::new("previewModelData");
pub const SUGGEST_BINDINGS: HandlerId<AdminOnly> = HandlerId::new("suggestBindings");
pub const COMPILE_MODEL: HandlerId<AdminOnly> = HandlerId::new("compileModel");
pub const CANCEL_MODEL_FIT: HandlerId<AdminOnly> = HandlerId::new("cancelModelFit");
pub const GET_MODEL_DRAWS: HandlerId<AdminOnly> = HandlerId::new("getModelDraws");
pub const GET_POSTERIOR_SUMMARY: HandlerId<AdminOnly> = HandlerId::new("getPosteriorSummary");
pub const DOWNLOAD_MODEL_RUN: HandlerId<AdminOnly> = HandlerId::new("downloadModelRun");
pub const WRITE_POSTERIOR: HandlerId<AdminOnly> = HandlerId::new("writePosterior");

// --- `admin::modules` ---

pub const LIST_MODULES: HandlerId<AdminOnly> = HandlerId::new("listModules");
pub const INSTALL_MODULE: HandlerId<AdminOnly> = HandlerId::new("installModule");
pub const UPDATE_MODULE: HandlerId<AdminOnly> = HandlerId::new("updateModule");
pub const DELETE_MODULE: HandlerId<AdminOnly> = HandlerId::new("deleteModule");
pub const RELOAD_MODULES: HandlerId<AdminOnly> = HandlerId::new("reloadModules");

// --- `admin::rows` ---

pub const LIST_ROWS: HandlerId<AdminOnly> = HandlerId::new("listRows");
pub const COUNT_ROWS: HandlerId<AdminOnly> = HandlerId::new("countRows");
pub const CREATE_ROW: HandlerId<AdminOnly> = HandlerId::new("createRow");
pub const UPDATE_ROW: HandlerId<AdminOnly> = HandlerId::new("updateRow");
pub const DELETE_ROW: HandlerId<AdminOnly> = HandlerId::new("deleteRow");
pub const DELETE_ALL_ROWS: HandlerId<AdminOnly> = HandlerId::new("deleteAllRows");
pub const GET_ROW_HISTORY: HandlerId<AdminOnly> = HandlerId::new("getRowHistory");
pub const LIST_DELETED_ROWS: HandlerId<AdminOnly> = HandlerId::new("listDeletedRows");
pub const GET_ROW_AS_OF: HandlerId<AdminOnly> = HandlerId::new("getRowAsOf");
pub const LIST_CHANGESETS: HandlerId<AdminOnly> = HandlerId::new("listChangesets");
pub const GET_CHANGESET: HandlerId<AdminOnly> = HandlerId::new("getChangeset");
pub const RESTORE_ROW: HandlerId<AdminOnly> = HandlerId::new("restoreRow");
pub const REVERT_ROW: HandlerId<AdminOnly> = HandlerId::new("revertRow");
pub const UNDO_CHANGESET: HandlerId<AdminOnly> = HandlerId::new("undoChangeset");
pub const EXPORT_TABLE_CSV: HandlerId<AdminOnly> = HandlerId::new("exportTableCsv");
pub const IMPORT_TABLE_CSV: HandlerId<AdminOnly> = HandlerId::new("importTableCsv");

// --- `admin::settings` ---

pub const GET_HELP: HandlerId<AdminOnly> = HandlerId::new("getHelp");
pub const GET_SETTINGS: HandlerId<AdminOnly> = HandlerId::new("getSettings");
pub const UPDATE_SETTINGS: HandlerId<AdminOnly> = HandlerId::new("updateSettings");
pub const LIST_O_AUTH_PROVIDERS: HandlerId<AdminOnly> = HandlerId::new("listOAuthProviders");
pub const GET_O_AUTH_PROVIDER_SPEC: HandlerId<AdminOnly> = HandlerId::new("getOAuthProviderSpec");
pub const CREATE_O_AUTH_PROVIDER: HandlerId<AdminOnly> = HandlerId::new("createOAuthProvider");
pub const UPDATE_O_AUTH_PROVIDER: HandlerId<AdminOnly> = HandlerId::new("updateOAuthProvider");
pub const DELETE_O_AUTH_PROVIDER: HandlerId<AdminOnly> = HandlerId::new("deleteOAuthProvider");
pub const GET_ADMIN_ADDRESS: HandlerId<AdminOnly> = HandlerId::new("getAdminAddress");
pub const CREATE_ADMIN_HANDOFF: HandlerId<AdminOnly> = HandlerId::new("createAdminHandoff");
pub const SEND_TEST_EMAIL: HandlerId<AdminOnly> = HandlerId::new("sendTestEmail");
pub const GET_PYTHON_STATUS: HandlerId<AdminOnly> = HandlerId::new("getPythonStatus");
pub const GET_SYSTEM_INFO: HandlerId<AdminOnly> = HandlerId::new("getSystemInfo");
pub const RESTART_SERVER: HandlerId<AdminOnly> = HandlerId::new("restartServer");

// --- `admin::streams` ---

pub const LIST_STREAM_PROVIDERS: HandlerId<AdminOnly> = HandlerId::new("listStreamProviders");
pub const LIST_STREAMS: HandlerId<AdminOnly> = HandlerId::new("listStreams");
pub const GET_STREAM: HandlerId<AdminOnly> = HandlerId::new("getStream");
pub const SAVE_STREAM: HandlerId<AdminOnly> = HandlerId::new("saveStream");
pub const DELETE_STREAM: HandlerId<AdminOnly> = HandlerId::new("deleteStream");
pub const STREAM_STATUS: HandlerId<AdminOnly> = HandlerId::new("streamStatus");

// --- `admin::tables` ---

pub const LIST_TABLES: HandlerId<AdminOnly> = HandlerId::new("listTables");
pub const CREATE_TABLE: HandlerId<AdminOnly> = HandlerId::new("createTable");
pub const CREATE_TABLE_FROM_CSV: HandlerId<AdminOnly> = HandlerId::new("createTableFromCsv");
pub const CREATE_TABLE_FROM_GEO_FILE: HandlerId<AdminOnly> =
    HandlerId::new("createTableFromGeoFile");
pub const UPDATE_TABLE: HandlerId<AdminOnly> = HandlerId::new("updateTable");
pub const DROP_TABLE: HandlerId<AdminOnly> = HandlerId::new("dropTable");
pub const RENAME_TABLE: HandlerId<AdminOnly> = HandlerId::new("renameTable");
pub const LIST_METADATA_TABLES: HandlerId<AdminOnly> = HandlerId::new("listMetadataTables");
pub const CREATE_METADATA_TABLE: HandlerId<AdminOnly> = HandlerId::new("createMetadataTable");
pub const LIST_TABLE_PROVIDERS: HandlerId<AdminOnly> = HandlerId::new("listTableProviders");
pub const CREATE_PROVIDED_TABLE: HandlerId<AdminOnly> = HandlerId::new("createProvidedTable");
pub const UPDATE_PROVIDED_TABLE: HandlerId<AdminOnly> = HandlerId::new("updateProvidedTable");
pub const DELETE_TABLE_SETTINGS: HandlerId<AdminOnly> = HandlerId::new("deleteTableSettings");
pub const LIST_ORPHAN_TABLE_SETTINGS: HandlerId<AdminOnly> =
    HandlerId::new("listOrphanTableSettings");

// --- `admin::translations` ---

pub const GET_TRANSLATIONS: HandlerId<AdminOnly> = HandlerId::new("getTranslations");
pub const SET_APPLICATION_LOCALES: HandlerId<AdminOnly> = HandlerId::new("setApplicationLocales");
pub const SAVE_TRANSLATIONS: HandlerId<AdminOnly> = HandlerId::new("saveTranslations");
pub const TRANSLATE_MISSING: HandlerId<AdminOnly> = HandlerId::new("translateMissing");
pub const GET_INSTANCE_TRANSLATIONS: HandlerId<AdminOnly> =
    HandlerId::new("getInstanceTranslations");
pub const SAVE_INSTANCE_TRANSLATIONS: HandlerId<AdminOnly> =
    HandlerId::new("saveInstanceTranslations");
pub const TRANSLATE_INSTANCE_MISSING: HandlerId<AdminOnly> =
    HandlerId::new("translateInstanceMissing");

// --- `admin::triggers` ---

pub const LIST_TRIGGERS: HandlerId<AdminOnly> = HandlerId::new("listTriggers");
pub const CREATE_TRIGGER: HandlerId<AdminOnly> = HandlerId::new("createTrigger");
pub const UPDATE_TRIGGER: HandlerId<AdminOnly> = HandlerId::new("updateTrigger");
pub const DELETE_TRIGGER: HandlerId<AdminOnly> = HandlerId::new("deleteTrigger");
pub const RUN_TRIGGER: HandlerId<AdminOnly> = HandlerId::new("runTrigger");
pub const TEST_RUN_TRIGGER: HandlerId<AdminOnly> = HandlerId::new("testRunTrigger");
pub const LIST_ACTIONS: HandlerId<AdminOnly> = HandlerId::new("listActions");
pub const GET_WORKFLOW: HandlerId<AdminOnly> = HandlerId::new("getWorkflow");
pub const SAVE_WORKFLOW: HandlerId<AdminOnly> = HandlerId::new("saveWorkflow");
pub const REVERT_WORKFLOW: HandlerId<AdminOnly> = HandlerId::new("revertWorkflow");
pub const LIST_WORKFLOW_RUNS: HandlerId<AdminOnly> = HandlerId::new("listWorkflowRuns");
pub const RESUME_RUN: HandlerId<AdminOnly> = HandlerId::new("resumeRun");
pub const CANCEL_RUN: HandlerId<AdminOnly> = HandlerId::new("cancelRun");
pub const RETRY_RUN: HandlerId<AdminOnly> = HandlerId::new("retryRun");

// --- `admin::users` ---

pub const LIST_ROLES: HandlerId<AdminOnly> = HandlerId::new("listRoles");
pub const CREATE_ROLE: HandlerId<AdminOnly> = HandlerId::new("createRole");
pub const SET_ROLE_SECOND_FACTOR: HandlerId<AdminOnly> = HandlerId::new("setRoleSecondFactor");
pub const DELETE_ROLE: HandlerId<AdminOnly> = HandlerId::new("deleteRole");
pub const LIST_USERS: HandlerId<AdminOnly> = HandlerId::new("listUsers");
pub const CREATE_USER: HandlerId<AdminOnly> = HandlerId::new("createUser");
pub const UPDATE_USER: HandlerId<AdminOnly> = HandlerId::new("updateUser");
pub const DELETE_USER: HandlerId<AdminOnly> = HandlerId::new("deleteUser");
pub const SET_USER_DISABLED: HandlerId<AdminOnly> = HandlerId::new("setUserDisabled");
pub const FORCE_LOGOUT_USER: HandlerId<AdminOnly> = HandlerId::new("forceLogoutUser");
pub const BECOME_USER: HandlerId<AdminOnly> = HandlerId::new("becomeUser");
pub const RESET_USER_SECOND_FACTOR: HandlerId<AdminOnly> = HandlerId::new("resetUserSecondFactor");
pub const SET_RANDOM_PASSWORD: HandlerId<AdminOnly> = HandlerId::new("setRandomPassword");
pub const LIST_USER_ROLE_GRANTS: HandlerId<AdminOnly> = HandlerId::new("listUserRoleGrants");
pub const GRANT_USER_ROLE: HandlerId<AdminOnly> = HandlerId::new("grantUserRole");
pub const REVOKE_USER_ROLE_GRANT: HandlerId<AdminOnly> = HandlerId::new("revokeUserRoleGrant");
pub const GET_USER_CREDITS: HandlerId<AdminOnly> = HandlerId::new("getUserCredits");
pub const GRANT_USER_CREDITS: HandlerId<AdminOnly> = HandlerId::new("grantUserCredits");
pub const SET_USER_CREDIT_THRESHOLD: HandlerId<AdminOnly> =
    HandlerId::new("setUserCreditThreshold");
pub const LIST_API_TOKENS: HandlerId<AdminOnly> = HandlerId::new("listApiTokens");
pub const CREATE_API_TOKEN: HandlerId<AdminOnly> = HandlerId::new("createApiToken");
pub const REVOKE_API_TOKEN: HandlerId<AdminOnly> = HandlerId::new("revokeApiToken");

// --- `admin::views` ---

pub const LIST_VIEWS: HandlerId<AdminOnly> = HandlerId::new("listViews");
pub const GET_VIEW: HandlerId<AdminOnly> = HandlerId::new("getView");
pub const SAVE_VIEW: HandlerId<AdminOnly> = HandlerId::new("saveView");
pub const DELETE_VIEW: HandlerId<AdminOnly> = HandlerId::new("deleteView");
pub const CREATE_VIEW: HandlerId<AdminOnly> = HandlerId::new("createView");
pub const VIEW_CONFIG_STEP: HandlerId<AdminOnly> = HandlerId::new("viewConfigStep");
pub const VIEW_REFERENCES: HandlerId<AdminOnly> = HandlerId::new("viewReferences");
pub const LIST_PAGES: HandlerId<AdminOnly> = HandlerId::new("listPages");
pub const GET_PAGE: HandlerId<AdminOnly> = HandlerId::new("getPage");
pub const SAVE_PAGE: HandlerId<AdminOnly> = HandlerId::new("savePage");
pub const DELETE_PAGE: HandlerId<AdminOnly> = HandlerId::new("deletePage");
pub const SAVE_VIEW_LAYOUT: HandlerId<AdminOnly> = HandlerId::new("saveViewLayout");
pub const SAVE_PAGE_LAYOUT: HandlerId<AdminOnly> = HandlerId::new("savePageLayout");
pub const PAGE_REFERENCES: HandlerId<AdminOnly> = HandlerId::new("pageReferences");
pub const LIST_LIBRARY: HandlerId<AdminOnly> = HandlerId::new("listLibrary");
pub const GET_LIBRARY_ITEM: HandlerId<AdminOnly> = HandlerId::new("getLibraryItem");
pub const CREATE_LIBRARY_ITEM: HandlerId<AdminOnly> = HandlerId::new("createLibraryItem");
pub const SAVE_LIBRARY_ITEM: HandlerId<AdminOnly> = HandlerId::new("saveLibraryItem");
pub const SAVE_LIBRARY_UPDATES: HandlerId<AdminOnly> = HandlerId::new("saveLibraryUpdates");
pub const DELETE_LIBRARY_ITEM: HandlerId<AdminOnly> = HandlerId::new("deleteLibraryItem");
pub const BUILDER_FIELD_PREVIEW: HandlerId<AdminOnly> = HandlerId::new("builderFieldPreview");
pub const BUILDER_FIELDVIEW_CONFIG_FORM: HandlerId<AdminOnly> =
    HandlerId::new("builderFieldviewConfigForm");
pub const BUILDER_VIEW_PREVIEW: HandlerId<AdminOnly> = HandlerId::new("builderViewPreview");
pub const BUILDER_PAGE_PREVIEW: HandlerId<AdminOnly> = HandlerId::new("builderPagePreview");
pub const BUILDER_DISTINCT_VALUES: HandlerId<AdminOnly> = HandlerId::new("builderDistinctValues");
pub const LIST_VIEW_PATTERNS: HandlerId<AdminOnly> = HandlerId::new("listViewPatterns");
pub const BUILDER_STATUS: HandlerId<AdminOnly> = HandlerId::new("builderStatus");

// --- `analytics` ---

pub const LIST_DATASETS: HandlerId<AnalyticsUi> = HandlerId::new("listDatasets");
pub const GET_DATASET: HandlerId<AnalyticsUi> = HandlerId::new("getDataset");
pub const CREATE_DATASET: HandlerId<AnalyticsUi> = HandlerId::new("createDataset");
pub const UPDATE_DATASET: HandlerId<AnalyticsUi> = HandlerId::new("updateDataset");
pub const DELETE_DATASET: HandlerId<AnalyticsUi> = HandlerId::new("deleteDataset");
pub const SHARE_DATASET: HandlerId<AnalyticsUi> = HandlerId::new("shareDataset");
pub const CLONE_DATASET: HandlerId<AnalyticsUi> = HandlerId::new("cloneDataset");
pub const LIST_PUBLIC_DATASETS: HandlerId<AdminOnly> = HandlerId::new("listPublicDatasets");
pub const INSTALL_PUBLIC_DATASET: HandlerId<AdminOnly> = HandlerId::new("installPublicDataset");
pub const GET_PUBLIC_DATASET_INSTALL: HandlerId<AdminOnly> =
    HandlerId::new("getPublicDatasetInstall");
pub const DATASET_USAGE: HandlerId<AnalyticsUi> = HandlerId::new("datasetUsage");
pub const DATASET_SHAPES: HandlerId<AnalyticsUi> = HandlerId::new("datasetShapes");
pub const VALIDATE_DATASET_OPERATION: HandlerId<AnalyticsUi> =
    HandlerId::new("validateDatasetOperation");
pub const READ_DATASET_STAGE: HandlerId<AnalyticsUi> = HandlerId::new("readDatasetStage");
pub const DATASET_COLUMN_VALUES: HandlerId<AnalyticsUi> = HandlerId::new("datasetColumnValues");
pub const LIST_DATASET_TABLES: HandlerId<AnalyticsUi> = HandlerId::new("listDatasetTables");
pub const RENDER_PLOT: HandlerId<AnalyticsUi> = HandlerId::new("renderPlot");
pub const RENDER_TABLE: HandlerId<AnalyticsUi> = HandlerId::new("renderTable");
pub const PLOT_GALLERY: HandlerId<AnalyticsUi> = HandlerId::new("plotGallery");
pub const RUN_TESTS: HandlerId<AnalyticsUi> = HandlerId::new("runTests");
pub const SUGGEST_PLOT: HandlerId<AnalyticsUi> = HandlerId::new("suggestPlot");
pub const LAYER_DATA: HandlerId<AnalyticsUi> = HandlerId::new("layerData");
pub const LAYER_TILE: HandlerId<AnalyticsUi> = HandlerId::new("layerTile");
pub const LAYER_ROWS: HandlerId<AnalyticsUi> = HandlerId::new("layerRows");
pub const SELECT_FEATURES: HandlerId<AnalyticsUi> = HandlerId::new("selectFeatures");
pub const SAVE_SELECTION: HandlerId<AnalyticsUi> = HandlerId::new("saveSelection");
pub const MAP_SETTINGS: HandlerId<AnalyticsUi> = HandlerId::new("mapSettings");
pub const SAVE_MAP_SETTINGS: HandlerId<AdminOnly> = HandlerId::new("saveMapSettings");
pub const ALLOW_MAP_HOST: HandlerId<AdminOnly> = HandlerId::new("allowMapHost");
pub const LIST_MAP_TOOLS: HandlerId<AnalyticsUi> = HandlerId::new("listMapTools");
pub const RUN_MAP_TOOL: HandlerId<AnalyticsUi> = HandlerId::new("runMapTool");
pub const SUGGEST_MAP: HandlerId<AnalyticsUi> = HandlerId::new("suggestMap");
pub const RENDER_MAP: HandlerId<AnalyticsUi> = HandlerId::new("renderMap");
pub const RENDER_PANEL: HandlerId<AnalyticsUi> = HandlerId::new("renderPanel");
pub const GET_MODEL_OUTPUTS: HandlerId<AdminOnly> = HandlerId::new("getModelOutputs");
pub const ANALYTICS_SHELL: HandlerId<AnalyticsUi> = HandlerId::new("analyticsShell");
pub const LIST_WORKSPACE_KINDS: HandlerId<AnalyticsUi> = HandlerId::new("listWorkspaceKinds");
pub const LIST_WORKSPACES: HandlerId<AnalyticsUi> = HandlerId::new("listWorkspaces");
pub const GET_WORKSPACE: HandlerId<AnalyticsUi> = HandlerId::new("getWorkspace");
pub const CREATE_WORKSPACE: HandlerId<AnalyticsUi> = HandlerId::new("createWorkspace");
pub const UPDATE_WORKSPACE: HandlerId<AnalyticsUi> = HandlerId::new("updateWorkspace");
pub const SHARE_WORKSPACE: HandlerId<AnalyticsUi> = HandlerId::new("shareWorkspace");
pub const SAVE_WORKSPACE_STATE: HandlerId<AnalyticsUi> = HandlerId::new("saveWorkspaceState");
pub const DELETE_WORKSPACE: HandlerId<AnalyticsUi> = HandlerId::new("deleteWorkspace");

// --- Handlers reached by a raw route rather than a typed endpoint ---
//
// The bytes of an upload, a download or a backup have no `TypeSchema`, so
// their routes are outside the endpoint set (`sc-server`'s raw-route
// registry); they still dispatch through the handler registry, under these.

pub const UPLOAD_FILE: HandlerId<AdminOnly> = HandlerId::new("uploadFile");
pub const DOWNLOAD_FILE: HandlerId<AdminOnly> = HandlerId::new("downloadFile");
pub const CREATE_BACKUP: HandlerId<AdminOnly> = HandlerId::new("createBackup");
pub const UPLOAD_BACKUP: HandlerId<AdminOnly> = HandlerId::new("uploadBackup");

#[cfg(test)]
mod tests {
    use crate::{AuthRequirement, admin_endpoints};

    /// Every declared admin endpoint carries the requirement of the id it was
    /// declared with: the admin set is all admin, but for the four pieces of
    /// sign-in plumbing.
    #[test]
    fn the_admin_set_is_admin_but_for_the_sign_in_plumbing() {
        let set = admin_endpoints();
        for ep in &set {
            let expected = match ep.name.as_str() {
                "login" | "authStatus" | "createFirstUser" => AuthRequirement::Public,
                "logout" => AuthRequirement::LoggedIn,
                _ => AuthRequirement::admin(),
            };
            assert_eq!(ep.auth, expected, "{}", ep.name);
        }
        assert_eq!(super::LIST_TABLES.name(), "listTables");
        assert_eq!(super::LIST_TABLES.requirement(), AuthRequirement::admin());
        assert_eq!(super::LOGIN.requirement(), AuthRequirement::Public);
    }
}
