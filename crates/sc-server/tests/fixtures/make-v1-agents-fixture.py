#!/usr/bin/env python3
"""Write saltcorn-v1-feldsparimport-agents.zip from saltcorn-v1-feldsparimport.zip.

    python3 crates/sc-server/tests/fixtures/make-v1-agents-fixture.py

The output is the real order-entry backup with an agent that uses **every
skill** of v1's agents plugin (@saltcorn/agents 1.0.4, checked out at
~/agents), and what those skills read, added to its pack.json — each in the
shape that plugin's own configFields() write (see README.md beside this script):

- the agent *EverySkill*, with one of each skill (two of Run JavaScript code,
  a tool and a button; two of Preload Data, a table and code);
- an `AgentLongTermMemory` table, as the Memory skill creates it, with three
  memories in `tables/AgentLongTermMemory.json`;
- a `chunks` table with a `PGVector` field (the pgvector plugin's type) of
  three dimensions, and a `prompts` table for the prompt picker;
- an alternative configuration *Fast* on the LLM plugin, for the model picker;
- a daily `consolidate_agent_memory` action.

Every other entry is copied byte for byte. Timestamps are fixed, so running the
script again writes the same file.
"""

import json
import pathlib
import zipfile

HERE = pathlib.Path(__file__).resolve().parent
SOURCE = HERE / "saltcorn-v1-feldsparimport.zip"
TARGET = HERE / "saltcorn-v1-feldsparimport-agents.zip"
# The time the source backup was taken (backup-info.json's backup_date).
STAMP = (2026, 10, 26, 10, 38, 45)


def field(name, type_, **extra):
    out = {
        "name": name,
        "label": name,
        "is_unique": False,
        "calculated": False,
        "stored": False,
        "expression": None,
        "type": type_,
        "attributes": {},
        "required": False,
        "primary_key": False,
        "refname": "",
        "description": "",
    }
    out.update(extra)
    return out


def table(name, fields):
    return {
        "name": name,
        "description": "",
        "is_user_group": False,
        "min_role_read": 1,
        "min_role_write": 1,
        "versioned": False,
        "has_sync_info": False,
        "provider_name": None,
        "provider_cfg": None,
        "ownership_formula": None,
        "rls_enabled": False,
        "fields": [field("id", "Integer", is_unique=True, required=True, primary_key=True)]
        + fields,
        "constraints": [],
        "ownership_field_name": None,
    }


def key(name, target, summary):
    return field(
        name,
        "Key",
        reftable_name=target,
        reftype="Integer",
        refname="id",
        attributes={"on_delete": "Set null", "include_fts": False, "summary_field": summary},
    )


TABLES = [
    table(
        "chunks",
        [
            field("text", "String"),
            field("embedding", "PGVector", attributes={"dimensions": 3}),
            key("doc", "document", "contents"),
        ],
    ),
    table("prompts", [field("title", "String"), field("body", "String")]),
    # As LongTermMemory.js's get_table() creates it.
    table(
        "AgentLongTermMemory",
        [
            field("run_id", "Integer"),
            key("user_id", "users", "email"),
            field("written_at", "Date"),
            field("agent_trigger_id", "Integer"),
            field("memory_type", "String"),
            field("personal", "Bool"),
            field("topic", "String"),
            field("contents", "String"),
        ],
    ),
]

ROWS = {
    "chunks": [{"id": 1, "text": "Orders ship on Mondays", "embedding": "[0.1,0.2,0.3]", "doc": None}],
    "prompts": [{"id": 1, "title": "Pirate", "body": "Speak like a pirate"}],
    "AgentLongTermMemory": [
        {"id": 1, "run_id": 4, "user_id": 1, "written_at": "2026-10-20T09:00:00.000Z",
         "agent_trigger_id": 3, "memory_type": "Episodic", "personal": True, "topic": None,
         "contents": "Prefers invoices by email"},
        {"id": 2, "run_id": 4, "user_id": None, "written_at": "2026-10-20T09:01:00.000Z",
         "agent_trigger_id": 3, "memory_type": "Episodic", "personal": False, "topic": None,
         "contents": "The warehouse is closed on the 24th"},
        {"id": 3, "run_id": None, "user_id": 1, "written_at": "2026-10-21T02:00:00.000Z",
         "agent_trigger_id": None, "memory_type": "PersonalPreferences", "personal": True,
         "topic": None, "contents": "The user prefers invoices by email."},
    ],
}

SKILLS = [
    {"skill_type": "Table", "table_name": "document", "query": "on", "insert": True,
     "hidden_fields": "", "list_view": "MyAgentView", "max_rows": "10", "add_sys_prompt": ""},
    {"skill_type": "Run JavaScript code", "mode": "Tool", "tool_name": "shout",
     "tool_description": "Upper-case a text", "js_code": "return text.toUpperCase()",
     "toolargs": [{"name": "text", "argtype": "string", "description": "the text"}],
     "add_sys_prompt": ""},
    {"skill_type": "Run JavaScript code", "mode": "Button", "button_label": "Count orders",
     "js_code": "return 3", "add_sys_prompt": ""},
    {"skill_type": "Trigger", "trigger_name": "MyWorkflow", "add_sys_prompt": ""},
    {"skill_type": "Subagent", "agent_name": "MyAgent", "handover_prompt": "You help EverySkill.",
     "handoff_prompt": "Summarise what it found."},
    {"skill_type": "Fetch", "cookiejar": True},
    {"skill_type": "Retrieval by full-text search", "mode": "Tool", "table_name": "document",
     "list_view": "", "hidden_fields": "", "doc_format": "", "add_sys_prompt": ""},
    {"skill_type": "Retrieval by embedding", "mode": "Tool", "vec_field": "chunks.embedding",
     "doc_relation": "", "list_view": "", "hidden_fields": "", "limit": "5", "doc_format": "{{text}}",
     "add_sys_prompt": ""},
    {"skill_type": "Preload Data", "data_source": "Table", "table_name": "prompts",
     "preload_query": "{title: title}", "add_sys_prompt": "Known prompts:", "contents_expr": "",
     "hidden_fields": "body"},
    {"skill_type": "Preload Data", "data_source": "Code", "code": "return 'Today is a weekday.'"},
    {"skill_type": "Memory", "inject_personal_prefs": True, "add_sys_prompt": ""},
    {"skill_type": "Model Context Protocol", "server_label": "Docs", "server_url": "http://127.0.0.1:9/mcp",
     "header_key": "Authorization", "header_value": "Bearer not-a-real-token"},
    {"skill_type": "Web search", "search_provider": "Tavily", "api_key": "tvly-not-a-real-key",
     "include_domains": "", "exclude_domains": ""},
    {"skill_type": "Generate and run JavaScript code", "tool_name": "compute",
     "tool_description": "Compute with code", "code_description": "The order table",
     "add_sys_prompt": "", "allow_fetch": False, "allow_table": True, "allow_functions": False,
     "follow_up_prompt": "", "table_read_only": True},
    {"skill_type": "Ask user question", "question_sys_prompt": "Ask when unsure."},
    {"skill_type": "Plan approval", "ini_sys_prompt": "Plan before acting.", "approval_prompt": "Carry it out."},
    {"skill_type": "Image generation", "size": "1024x1024", "quality": "auto", "format": "png",
     "transparent": False},
    {"skill_type": "Text to speech", "voice": "nova", "speed": 1, "format": "mp3", "instructions": ""},
    {"skill_type": "External Skill (SKILL.md)", "skills_dir": "/srv/saltcorn/skills/invoices"},
    {"skill_type": "Model picker", "placeholder": "", "default_label": ""},
    {"skill_type": "Prompt picker", "placeholder": "Style", "source": "Table", "run_on_select": False,
     "table": "prompts", "label_field": "title", "prompt_field": "body", "query": ""},
    {"skill_type": "Compaction", "trigger_tokens": 100000},
    {"skill_type": "Adaptive Feedback"},
]

TRIGGERS = [
    {"name": "EverySkill", "description": "Uses every skill", "action": "Agent",
     "when_trigger": "Never", "configuration": {"model": "", "sys_prompt": "You are thorough.",
                                                "skills": SKILLS},
     "channel": None, "min_role": 100},
    {"name": "ConsolidateMemories", "description": "", "action": "consolidate_agent_memory",
     "when_trigger": "Daily", "configuration": {}, "channel": None, "min_role": 100},
]


def edit(pack):
    pack["tables"].extend(TABLES)
    pack["triggers"].extend(TRIGGERS)
    llm = next(p for p in pack["plugins"] if p["name"] == "large-language-model")
    llm["configuration"]["alt_aisdk_configs"].append(
        {"name": "Fast", "model": "gpt-5-mini", "api_key": "", "alt_provider": "OpenAI",
         "embed_model": "", "reasoning_effort": ""}
    )
    return pack


def main():
    with zipfile.ZipFile(SOURCE) as source, zipfile.ZipFile(TARGET, "w", zipfile.ZIP_DEFLATED) as out:
        for info in source.infolist():
            data = source.read(info.filename)
            if info.filename == "pack.json":
                data = json.dumps(edit(json.loads(data))).encode()
            out.writestr(zipfile.ZipInfo(info.filename, info.date_time), data, zipfile.ZIP_DEFLATED)
        for name, rows in ROWS.items():
            out.writestr(
                zipfile.ZipInfo(f"tables/{name}.json", STAMP), json.dumps(rows).encode(), zipfile.ZIP_DEFLATED
            )


if __name__ == "__main__":
    main()
