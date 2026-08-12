//! The AI's room peer: read/write an app's shared CRDT room (automerge), so the
//! bot can co-edit `write` room variables like any conversation participant.
//! Uses the SAME automerge crate/version the API uses (`compact_room_log`), so
//! change blobs interop with the web's `@automerge/automerge`. The server stays
//! a dumb relay (`roomChanges` / `roomChange`); all CRDT logic lives here.
//!
//! Access is an OPTIMISTIC CONVENTION declared in the app manifest's `room`
//! schema (`{ key: "read" | "write" }`, default `read`). The caller checks the
//! mode before `set_room_keys`; this module just moves bytes + JSON.

#![allow(dead_code)] // wired into the agent turn loop in a follow-up.

use anyhow::{Context, Result};
use automerge::transaction::Transactable;
use automerge::{AutoCommit, ObjId, ObjType, ReadDoc, ScalarValue, Value, ROOT};
use base64::{engine::general_purpose::STANDARD, Engine};
use clap::Subcommand;
use serde_json::{json, Map, Value as J};

use crate::client::Client;

/// Fetch + fold all change blobs for `(conv, app)` into one automerge doc.
pub async fn load_room(client: &Client, conv: &str, app: &str) -> Result<AutoCommit> {
    let resp = client.room_changes(conv, app).await?;
    let mut doc = AutoCommit::new();
    if let Some(arr) = resp.get("changes").and_then(|c| c.as_array()) {
        for b64 in arr.iter().filter_map(|v| v.as_str()) {
            if let Ok(bytes) = STANDARD.decode(b64) {
                let _ = doc.load_incremental(&bytes); // skip undecodable, like the server
            }
        }
    }
    Ok(doc)
}

/// The room's top-level map as JSON — what the AI is shown for context.
pub fn room_to_json(doc: &AutoCommit) -> J {
    obj_to_json(doc, &ROOT)
}

fn obj_to_json(doc: &AutoCommit, obj: &ObjId) -> J {
    let is_list = matches!(doc.object_type(obj), Ok(ObjType::List) | Ok(ObjType::Text));
    if is_list {
        let len = doc.length(obj);
        let mut arr = Vec::with_capacity(len);
        for i in 0..len {
            if let Ok(Some((val, child))) = doc.get(obj, i) {
                arr.push(val_to_json(doc, val, child));
            }
        }
        J::Array(arr)
    } else {
        let mut m = Map::new();
        for key in doc.keys(obj) {
            if let Ok(Some((val, child))) = doc.get(obj, key.as_str()) {
                m.insert(key, val_to_json(doc, val, child));
            }
        }
        J::Object(m)
    }
}

fn val_to_json(doc: &AutoCommit, val: Value, child: ObjId) -> J {
    match val {
        Value::Object(_) => obj_to_json(doc, &child),
        Value::Scalar(s) => scalar_to_json(&s),
    }
}

fn scalar_to_json(s: &ScalarValue) -> J {
    match s {
        ScalarValue::Str(x) => J::String(x.to_string()),
        ScalarValue::Int(x) => json!(x),
        ScalarValue::Uint(x) => json!(x),
        ScalarValue::F64(x) => json!(x),
        ScalarValue::Boolean(x) => J::Bool(*x),
        ScalarValue::Counter(c) => json!(i64::from(c)),
        ScalarValue::Timestamp(x) => json!(x),
        ScalarValue::Bytes(b) => J::String(STANDARD.encode(b)),
        ScalarValue::Null => J::Null,
        ScalarValue::Unknown { .. } => J::Null,
    }
}

/// Apply `fields` (top-level key → JSON value) to the room root map and push the
/// resulting change blob via the relay. For the AI's `write` ops — the CALLER
/// must already have checked the manifest marks each key `write`.
pub async fn set_room_keys(
    client: &Client,
    conv: &str,
    app: &str,
    fields: Map<String, J>,
) -> Result<()> {
    let mut doc = load_room(client, conv, app).await?;
    let before = doc.get_heads();
    for (k, v) in fields {
        put_json(&mut doc, &ROOT, Prop::Key(k), v)?;
    }
    let changes: Vec<String> = doc
        .get_changes(&before)
        .into_iter()
        .map(|c| STANDARD.encode(c.raw_bytes()))
        .collect();
    if !changes.is_empty() {
        client.room_change(conv, app, changes).await?;
    }
    Ok(())
}

enum Prop {
    Key(String),
    Idx(usize),
}

/// Recursively materialize a JSON value into the automerge doc at `obj`/`prop`.
fn put_json(doc: &mut AutoCommit, obj: &ObjId, prop: Prop, v: J) -> Result<()> {
    match v {
        J::Null => set_scalar(doc, obj, prop, ScalarValue::Null)?,
        J::Bool(b) => set_scalar(doc, obj, prop, ScalarValue::Boolean(b))?,
        J::Number(n) => {
            if let Some(i) = n.as_i64() {
                set_scalar(doc, obj, prop, ScalarValue::Int(i))?;
            } else {
                set_scalar(doc, obj, prop, ScalarValue::F64(n.as_f64().unwrap_or(0.0)))?;
            }
        }
        J::String(s) => set_scalar(doc, obj, prop, ScalarValue::Str(s.into()))?,
        J::Array(items) => {
            let list = put_obj(doc, obj, prop, ObjType::List)?;
            for (i, item) in items.into_iter().enumerate() {
                doc.insert(&list, i, ScalarValue::Null)?;
                put_json(doc, &list, Prop::Idx(i), item)?;
            }
        }
        J::Object(map) => {
            let m = put_obj(doc, obj, prop, ObjType::Map)?;
            for (k, val) in map {
                put_json(doc, &m, Prop::Key(k), val)?;
            }
        }
    }
    Ok(())
}

fn set_scalar(doc: &mut AutoCommit, obj: &ObjId, prop: Prop, s: ScalarValue) -> Result<()> {
    match prop {
        Prop::Key(k) => doc.put(obj, k, s)?,
        Prop::Idx(i) => doc.put(obj, i, s)?,
    }
    Ok(())
}

fn put_obj(doc: &mut AutoCommit, obj: &ObjId, prop: Prop, t: ObjType) -> Result<ObjId> {
    Ok(match prop {
        Prop::Key(k) => doc.put_object(obj, k, t)?,
        Prop::Idx(i) => doc.put_object(obj, i, t)?,
    })
}

// ───────────────── `mafold room` CLI (backs the room skill) ─────────────────

#[derive(Subcommand)]
pub enum RoomCmd {
    /// List app rooms in this conversation + each variable's read/write mode.
    List {
        #[arg(long, env = "MAFOLD_CONV")]
        conv: String,
    },
    /// Print an app's shared room state as JSON.
    Get {
        /// App id (`owner/slug`). Omit when the conversation has exactly one room.
        app: Option<String>,
        #[arg(long, env = "MAFOLD_CONV")]
        conv: String,
    },
    /// Set a `write` room variable to a JSON value (read-only keys are refused).
    Set {
        /// App id (`owner/slug`).
        app: String,
        /// Variable name.
        key: String,
        /// JSON value, e.g. '["milk","eggs"]', '42', 'true', '"hi"'.
        value: String,
        #[arg(long, env = "MAFOLD_CONV")]
        conv: String,
    },
}

pub async fn run(cmd: RoomCmd, base: String, token: Option<String>) -> Result<()> {
    let client = Client::new(base, token.unwrap_or_default());
    match cmd {
        RoomCmd::List { conv } => {
            let rooms = app_rooms(&client, &conv).await?;
            if rooms.is_empty() {
                println!("(no app rooms in this conversation)");
            }
            for (id, schema) in rooms {
                let cols: Vec<String> = schema.iter().map(|(k, m)| format!("{k}:{m}")).collect();
                println!("{id}   {{ {} }}", cols.join(", "));
            }
        }
        RoomCmd::Get { app, conv } => {
            let app = resolve_app(&client, &conv, app).await?;
            let doc = load_room(&client, &conv, &app).await?;
            println!("{}", serde_json::to_string_pretty(&room_to_json(&doc))?);
        }
        RoomCmd::Set {
            app,
            key,
            value,
            conv,
        } => {
            let schema = room_schema(&client, &conv, &app).await?;
            let mode = schema_mode(&schema, &key);
            if mode != "write" {
                let cols: Vec<String> = schema.iter().map(|(k, m)| format!("{k}:{m}")).collect();
                anyhow::bail!(
                    "`{key}` is `{mode}` — only `write` variables are editable (the app owns read-only ones). \
This app's room: {{ {} }}",
                    cols.join(", ")
                );
            }
            let v: J = serde_json::from_str(&value)
                .with_context(|| format!("value must be valid JSON (got `{value}`)"))?;
            let mut fields = Map::new();
            fields.insert(key.clone(), v);
            set_room_keys(&client, &conv, &app, fields).await?;
            println!("ok — set `{key}` in {app}");
        }
    }
    Ok(())
}

/// (app_id, room schema) for every installed app in the conv that declares a
/// `room` in its manifest.
async fn app_rooms(client: &Client, conv: &str) -> Result<Vec<(String, Vec<(String, String)>)>> {
    let resp = client.list_installs(conv).await?;
    let mut out = Vec::new();
    if let Some(items) = resp.get("items").and_then(|i| i.as_array()) {
        for it in items {
            let id = it
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if id.is_empty() {
                continue;
            }
            if let Some(room) = it
                .get("manifest")
                .and_then(|m| m.get("room"))
                .and_then(|r| r.as_object())
            {
                let schema = room
                    .iter()
                    .map(|(k, v)| (k.clone(), v.as_str().unwrap_or("read").to_string()))
                    .collect();
                out.push((id, schema));
            }
        }
    }
    Ok(out)
}

async fn room_schema(
    client: &Client,
    conv: &str,
    app: &str,
) -> Result<std::collections::BTreeMap<String, String>> {
    app_rooms(client, conv)
        .await?
        .into_iter()
        .find(|(id, _)| id == app)
        .map(|(_, s)| s.into_iter().collect())
        .ok_or_else(|| anyhow::anyhow!("app `{app}` isn't installed here, or declares no room"))
}

/// The declared mode for `key`. An EXACT schema entry wins; otherwise a
/// `prefix:*` wildcard entry matches (so `issue:*` covers `issue:abc`, and a
/// bare `*` covers everything). Default `read` when nothing matches — apps that
/// use per-entity keys (e.g. `issue:<id>`) can't enumerate ids up front, so the
/// wildcard is how they mark a whole family editable.
fn schema_mode<'a>(schema: &'a std::collections::BTreeMap<String, String>, key: &str) -> &'a str {
    if let Some(m) = schema.get(key) {
        return m.as_str();
    }
    for (k, m) in schema {
        if let Some(prefix) = k.strip_suffix('*') {
            if key.starts_with(prefix) {
                return m.as_str();
            }
        }
    }
    "read"
}

/// A compact per-turn context block: the apps installed in THIS conversation
/// and, for each that declares one, its room + editable schema. Injected into
/// the bot's prompt so it knows what it can operate via `mafold room`. GENERIC —
/// it reflects whatever is installed and whatever each app declares, with zero
/// per-app knowledge. `None` when nothing is installed (no prompt overhead).
pub async fn context_block(client: &Client, conv: &str) -> Result<Option<String>> {
    let resp = client.list_installs(conv).await?;
    let items = match resp.get("items").and_then(|i| i.as_array()) {
        Some(a) if !a.is_empty() => a,
        _ => return Ok(None),
    };
    let mut apps: Vec<String> = Vec::new();
    let mut rooms: Vec<String> = Vec::new();
    for it in items {
        let id = it.get("id").and_then(|v| v.as_str()).unwrap_or("");
        if id.is_empty() {
            continue;
        }
        let manifest = it.get("manifest");
        let name = manifest
            .and_then(|m| m.get("name"))
            .and_then(|v| v.as_str())
            .unwrap_or(id);
        apps.push(format!("• {name} ({id})"));
        if let Some(room) = manifest
            .and_then(|m| m.get("room"))
            .and_then(|r| r.as_object())
        {
            let cols: Vec<String> = room
                .iter()
                .map(|(k, v)| format!("{k}:{}", v.as_str().unwrap_or("read")))
                .collect();
            rooms.push(format!("• {id}   {{ {} }}", cols.join(", ")));
        }
    }
    if apps.is_empty() {
        return Ok(None);
    }
    let mut s = String::from(
        "[AVAILABLE APPS & ROOMS — mini-apps installed in THIS conversation. Their shared \
state lives in a co-edited room you can read/write with the `mafold room` CLI (the \
conversation is preset via MAFOLD_CONV). Reach for it when the user asks to view or \
change one of these apps' data.]\nApps:\n",
    );
    for a in &apps {
        s.push_str(a);
        s.push('\n');
    }
    if rooms.is_empty() {
        s.push_str("Rooms: (none of these apps declares a co-editable room)\n");
    } else {
        s.push_str(
            "Rooms — `id { variable:mode }`; only `write` variables are editable, and a `key:*` \
entry matches any key with that prefix (e.g. `issue:*` covers `issue:abc`):\n",
        );
        for r in &rooms {
            s.push_str(r);
            s.push('\n');
        }
    }
    s.push_str("[END AVAILABLE APPS & ROOMS]");
    Ok(Some(s))
}

async fn resolve_app(client: &Client, conv: &str, app: Option<String>) -> Result<String> {
    if let Some(a) = app {
        return Ok(a);
    }
    let rooms = app_rooms(client, conv).await?;
    match rooms.len() {
        1 => Ok(rooms.into_iter().next().unwrap().0),
        0 => anyhow::bail!("no app rooms in this conversation"),
        _ => {
            let ids: Vec<String> = rooms.into_iter().map(|(id, _)| id).collect();
            anyhow::bail!("multiple app rooms — pass one: {}", ids.join(", "))
        }
    }
}

/// Install the `mafold-room` skill into the agent's Claude Code skills dir so the
/// bot's claude discovers it. Idempotent; called on daemon startup.
pub fn install_skill() -> Result<()> {
    let home = std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("no HOME"))?;
    let dir = home.join(".claude").join("skills").join("mafold-room");
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join("SKILL.md"), SKILL_MD)?;
    Ok(())
}

const SKILL_MD: &str = r#"---
name: mafold-room
description: Read or edit an app's shared room data in the current Mafold conversation — e.g. a co-managed todo list, board, or counter. Use whenever the user asks to view, add, change, complete, or check items in a group app's shared state.
---

# Editing a Mafold app's shared room

Apps in this conversation keep shared state in a **room** — variables (e.g. a
`todolist`) that the app AND everyone in the chat (including you) co-edit, via an
optimistic CRDT. You touch it through the `mafold room` CLI. The current
conversation is already set in `MAFOLD_CONV`, so you never pass it.

## 1. See what rooms exist
```
mafold room list
```
Prints each installed app's room + its variable schema, e.g.
`acme/todo   { todolist:write, archived:read }`.
- `write` — you (and participants) may change this variable.
- `read`  — only the app itself writes it; you can read but NOT change it.
- A key ending in `*` is a wildcard: `issue:*:write` means every `issue:<id>`
  key (e.g. `issue:abc`) is writable. Apps with many items use this so each
  item is its own key — edit ONE item without rewriting the others.

## 2. Read current state
```
mafold room get acme/todo
```
Prints the room as JSON.

## 3. Change a `write` variable
```
mafold room set acme/todo todolist '[{"text":"buy milk","done":false}]'
```
- The value is JSON (array / object / string / number / bool).
- To add or edit items: `get` first, modify the JSON, then `set` the whole variable.
- Setting a `read` variable is refused — don't try; tell the user it's app-owned.

## Rules
- Only change variables the schema marks `write`.
- Never invent app ids — find them with `mafold room list`.
- After a change, briefly tell the user what you changed.
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_room_roundtrips_and_reloads() {
        // Build a doc the way the AI would write one.
        let mut doc = AutoCommit::new();
        let fields: Map<String, J> = serde_json::from_value(json!({
            "title": "groceries",
            "count": 2,
            "done": false,
            "items": ["milk", "eggs"],
        }))
        .unwrap();
        for (k, v) in fields {
            put_json(&mut doc, &ROOT, Prop::Key(k), v).unwrap();
        }

        // Dump → JSON matches.
        let out = room_to_json(&doc);
        assert_eq!(out["title"], "groceries");
        assert_eq!(out["count"], 2);
        assert_eq!(out["done"], false);
        assert_eq!(out["items"], json!(["milk", "eggs"]));

        // Save → base64 → load_incremental (the exact decode path load_room uses,
        // and the same one the API + web exchange over the wire) → identical.
        let blob = STANDARD.encode(doc.save());
        let mut doc2 = AutoCommit::new();
        doc2.load_incremental(&STANDARD.decode(blob).unwrap())
            .unwrap();
        assert_eq!(room_to_json(&doc2), out);
    }
}
