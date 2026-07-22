use crate::provider::{Message, Usage};
use anyhow::{Context, Result};
use directories::BaseDirs;
use rusqlite::{Connection, OptionalExtension, params};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;
use uuid::Uuid;

pub struct Database {
    connection: Connection,
    path: PathBuf,
}

#[derive(Debug)]
pub struct Session {
    pub id: String,
    pub title: String,
    pub provider: String,
    pub model: String,
}

pub struct SessionSummary {
    pub id: String,
    pub title: String,
    pub message_count: i64,
    pub total_tokens: i64,
    pub updated_at: i64,
}

#[derive(Default)]
pub struct SessionStats {
    pub request_count: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub total_tokens: i64,
    pub last_input_tokens: Option<i64>,
}

impl Database {
    pub fn open() -> Result<Self> {
        let data_dir = data_dir()?;
        fs::create_dir_all(&data_dir)
            .with_context(|| format!("failed to create {}", data_dir.display()))?;
        let path = data_dir.join("kamui.db");
        let connection = Connection::open(&path)
            .with_context(|| format!("failed to open {}", path.display()))?;
        Self::initialize(connection, path)
    }

    fn initialize(connection: Connection, path: PathBuf) -> Result<Self> {
        connection.busy_timeout(Duration::from_secs(5))?;
        connection.execute_batch(
            "PRAGMA foreign_keys = ON;
             PRAGMA journal_mode = WAL;
             CREATE TABLE IF NOT EXISTS sessions (
                 id TEXT PRIMARY KEY,
                 title TEXT NOT NULL,
                 provider TEXT NOT NULL,
                 model TEXT NOT NULL,
                 created_at INTEGER NOT NULL DEFAULT (unixepoch()),
                 updated_at INTEGER NOT NULL DEFAULT (unixepoch())
             );
             CREATE TABLE IF NOT EXISTS messages (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
                 role TEXT NOT NULL CHECK (role IN ('system', 'user', 'assistant')),
                 content TEXT NOT NULL,
                 created_at INTEGER NOT NULL DEFAULT (unixepoch())
             );
             CREATE INDEX IF NOT EXISTS messages_session_id ON messages(session_id, id);
             CREATE TABLE IF NOT EXISTS usage_records (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
                 input_tokens INTEGER NOT NULL,
                 output_tokens INTEGER NOT NULL,
                 total_tokens INTEGER NOT NULL,
                 finish_reason TEXT NOT NULL,
                 created_at INTEGER NOT NULL DEFAULT (unixepoch())
             );
             CREATE INDEX IF NOT EXISTS usage_session_id ON usage_records(session_id, id);",
        )?;
        let version: i64 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if version < 1 {
            connection.execute_batch("PRAGMA user_version = 1;")?;
        }
        if version < 2 {
            connection.execute_batch(
                "ALTER TABLE usage_records
                 ADD COLUMN kind TEXT NOT NULL DEFAULT 'chat';
                 PRAGMA user_version = 2;",
            )?;
        }
        Ok(Self { connection, path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn create_session(&self, provider: &str, model: &str) -> Result<Session> {
        let session = Session {
            id: Uuid::new_v4().to_string(),
            title: "New chat".to_string(),
            provider: provider.to_string(),
            model: model.to_string(),
        };
        self.connection.execute(
            "INSERT INTO sessions (id, title, provider, model) VALUES (?1, ?2, ?3, ?4)",
            params![session.id, session.title, session.provider, session.model],
        )?;
        Ok(session)
    }

    pub fn find_session(&self, id_prefix: &str) -> Result<Option<Session>> {
        let pattern = format!("{id_prefix}%");
        let mut statement = self.connection.prepare(
            "SELECT id, title, provider, model FROM sessions
             WHERE id LIKE ?1 ORDER BY updated_at DESC LIMIT 2",
        )?;
        let sessions = statement
            .query_map([pattern], session_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(if sessions.len() == 1 {
            sessions.into_iter().next()
        } else {
            None
        })
    }

    pub fn list_sessions(&self) -> Result<Vec<SessionSummary>> {
        let mut statement = self.connection.prepare(
            "SELECT s.id, s.title,
                    (SELECT COUNT(*) FROM messages WHERE session_id = s.id),
                    (SELECT COALESCE(SUM(total_tokens), 0)
                     FROM usage_records WHERE session_id = s.id),
                    s.updated_at
             FROM sessions s
             WHERE EXISTS (SELECT 1 FROM messages WHERE session_id = s.id)
             ORDER BY s.updated_at DESC, s.rowid DESC",
        )?;
        let rows = statement.query_map([], |row| {
            Ok(SessionSummary {
                id: row.get(0)?,
                title: row.get(1)?,
                message_count: row.get(2)?,
                total_tokens: row.get(3)?,
                updated_at: row.get(4)?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub fn load_messages(&self, session_id: &str) -> Result<Vec<Message>> {
        let mut statement = self
            .connection
            .prepare("SELECT role, content FROM messages WHERE session_id = ?1 ORDER BY id")?;
        let rows = statement.query_map([session_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        rows.map(|row| {
            let (role, content) = row?;
            Message::from_parts(&role, content)
        })
        .collect()
    }

    pub fn save_exchange(
        &self,
        session_id: &str,
        user: &Message,
        assistant: &Message,
        usage: &Usage,
        finish_reason: &str,
    ) -> Result<()> {
        let input_tokens =
            i64::try_from(usage.prompt_tokens).context("input token count overflow")?;
        let output_tokens =
            i64::try_from(usage.completion_tokens).context("output token count overflow")?;
        let total_tokens =
            i64::try_from(usage.total_tokens).context("total token count overflow")?;
        let transaction = self.connection.unchecked_transaction()?;
        transaction.execute(
            "INSERT INTO messages (session_id, role, content) VALUES (?1, ?2, ?3)",
            params![session_id, user.role_name(), user.content],
        )?;
        transaction.execute(
            "INSERT INTO messages (session_id, role, content) VALUES (?1, ?2, ?3)",
            params![session_id, assistant.role_name(), assistant.content],
        )?;
        transaction.execute(
            "INSERT INTO usage_records
             (session_id, input_tokens, output_tokens, total_tokens, finish_reason, kind)
             VALUES (?1, ?2, ?3, ?4, ?5, 'chat')",
            params![
                session_id,
                input_tokens,
                output_tokens,
                total_tokens,
                finish_reason
            ],
        )?;
        transaction.execute(
            "UPDATE sessions SET
                 title = CASE WHEN title = 'New chat' THEN ?2 ELSE title END,
                 updated_at = unixepoch()
             WHERE id = ?1",
            params![session_id, make_title(&user.content)],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn save_generated_title(
        &self,
        session_id: &str,
        title: &str,
        usage: &Usage,
        finish_reason: &str,
    ) -> Result<()> {
        let input_tokens =
            i64::try_from(usage.prompt_tokens).context("input token count overflow")?;
        let output_tokens =
            i64::try_from(usage.completion_tokens).context("output token count overflow")?;
        let total_tokens =
            i64::try_from(usage.total_tokens).context("total token count overflow")?;
        let transaction = self.connection.unchecked_transaction()?;
        transaction.execute(
            "UPDATE sessions SET title = ?2, updated_at = unixepoch() WHERE id = ?1",
            params![session_id, title],
        )?;
        transaction.execute(
            "INSERT INTO usage_records
             (session_id, input_tokens, output_tokens, total_tokens, finish_reason, kind)
             VALUES (?1, ?2, ?3, ?4, ?5, 'title')",
            params![
                session_id,
                input_tokens,
                output_tokens,
                total_tokens,
                finish_reason
            ],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn session_stats(&self, session_id: &str) -> Result<SessionStats> {
        let mut stats = self.connection.query_row(
            "SELECT COUNT(*) FILTER (WHERE kind = 'chat'), COALESCE(SUM(input_tokens), 0),
                    COALESCE(SUM(output_tokens), 0), COALESCE(SUM(total_tokens), 0)
             FROM usage_records WHERE session_id = ?1",
            [session_id],
            |row| {
                Ok(SessionStats {
                    request_count: row.get(0)?,
                    input_tokens: row.get(1)?,
                    output_tokens: row.get(2)?,
                    total_tokens: row.get(3)?,
                    last_input_tokens: None,
                })
            },
        )?;
        stats.last_input_tokens = self
            .connection
            .query_row(
                "SELECT input_tokens FROM usage_records
                 WHERE session_id = ?1 AND kind = 'chat' ORDER BY id DESC LIMIT 1",
                [session_id],
                |row| row.get(0),
            )
            .optional()?;
        Ok(stats)
    }

    pub fn delete_session(&self, session_id: &str) -> Result<()> {
        self.connection
            .execute("DELETE FROM sessions WHERE id = ?1", [session_id])?;
        Ok(())
    }
}

fn data_dir() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("KAMUI_DATA_DIR") {
        return Ok(PathBuf::from(path));
    }
    BaseDirs::new()
        .map(|dirs| dirs.data_local_dir().join("kamui"))
        .context("could not determine the operating system data directory")
}

fn session_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Session> {
    Ok(Session {
        id: row.get(0)?,
        title: row.get(1)?,
        provider: row.get(2)?,
        model: row.get(3)?,
    })
}

fn make_title(content: &str) -> String {
    let mut title: String = content.chars().take(40).collect();
    if content.chars().count() > 40 {
        title.push_str("...");
    }
    title
}

#[cfg(test)]
mod tests {
    use super::*;

    fn database() -> Database {
        Database::initialize(
            Connection::open_in_memory().unwrap(),
            PathBuf::from(":memory:"),
        )
        .unwrap()
    }

    #[test]
    fn persists_session_messages_and_usage() {
        let database = database();
        let session = database.create_session("test", "model").unwrap();
        database
            .save_exchange(
                &session.id,
                &Message::user("Explain Rust ownership"),
                &Message::assistant("Ownership tracks values."),
                &Usage {
                    prompt_tokens: 10,
                    completion_tokens: 5,
                    total_tokens: 15,
                },
                "stop",
            )
            .unwrap();

        let messages = database.load_messages(&session.id).unwrap();
        let stats = database.session_stats(&session.id).unwrap();
        let resumed = database.find_session(&session.id).unwrap().unwrap();

        assert_eq!(messages.len(), 2);
        assert_eq!(stats.request_count, 1);
        assert_eq!(stats.total_tokens, 15);
        assert_eq!(stats.last_input_tokens, Some(10));
        assert_eq!(resumed.title, "Explain Rust ownership");
    }

    #[test]
    fn deleting_session_cascades_related_data() {
        let database = database();
        let session = database.create_session("test", "model").unwrap();
        database
            .save_exchange(
                &session.id,
                &Message::user("hello"),
                &Message::assistant("hi"),
                &Usage::default(),
                "stop",
            )
            .unwrap();

        database.delete_session(&session.id).unwrap();

        assert!(database.find_session(&session.id).unwrap().is_none());
        assert!(database.load_messages(&session.id).unwrap().is_empty());
    }

    #[test]
    fn session_summary_does_not_multiply_usage_by_message_count() {
        let database = database();
        let session = database.create_session("test", "model").unwrap();
        database
            .save_exchange(
                &session.id,
                &Message::user("hello"),
                &Message::assistant("hi"),
                &Usage {
                    prompt_tokens: 4,
                    completion_tokens: 2,
                    total_tokens: 6,
                },
                "stop",
            )
            .unwrap();

        let summaries = database.list_sessions().unwrap();

        assert_eq!(summaries[0].message_count, 2);
        assert_eq!(summaries[0].total_tokens, 6);
    }

    #[test]
    fn session_list_hides_empty_sessions() {
        let database = database();
        database.create_session("test", "model").unwrap();

        assert!(database.list_sessions().unwrap().is_empty());
    }
}
