use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone)]
pub struct Database {
    conn: Arc<Mutex<sqlite::Connection>>,
}

impl Database {
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self, String> {
        let conn = sqlite::open(path).map_err(|e| e.to_string())?;
        let db = Self {
            conn: Arc::new(Mutex::new(conn)),
        };
        db.init_schema()?;
        db.seed_default_password()?;
        Ok(db)
    }

    fn init_schema(&self) -> Result<(), String> {
        let conn = self.conn.lock().map_err(|e| e.to_string())?;
        conn.execute("PRAGMA journal_mode = WAL; PRAGMA busy_timeout = 5000;")
            .map_err(|e| e.to_string())?;
        conn.execute(
            "CREATE TABLE IF NOT EXISTS config (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS admin_sessions (
                token TEXT PRIMARY KEY,
                created_at INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS blacklist (
                domain TEXT PRIMARY KEY,
                created_at INTEGER NOT NULL
            );",
        )
        .map_err(|e| e.to_string())?;
        Ok(())
    }

    fn seed_default_password(&self) -> Result<(), String> {
        let conn = self.conn.lock().map_err(|e| e.to_string())?;
        let mut statement = conn
            .prepare("SELECT value FROM config WHERE key = 'admin_password_hash'")
            .map_err(|e| e.to_string())?;
        if statement.next().map_err(|e| e.to_string())? == sqlite::State::Done {
            let hash = bcrypt::hash("admin", bcrypt::DEFAULT_COST)
                .map_err(|e| e.to_string())?;
            let mut insert = conn
                .prepare("INSERT INTO config (key, value) VALUES ('admin_password_hash', ?)")
                .map_err(|e| e.to_string())?;
            insert.bind((1, hash.as_str())).map_err(|e| e.to_string())?;
            insert.next().map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    pub fn get_config(&self, key: &str) -> Result<Option<String>, String> {
        let conn = self.conn.lock().map_err(|e| e.to_string())?;
        let mut stmt = conn
            .prepare("SELECT value FROM config WHERE key = ?")
            .map_err(|e| e.to_string())?;
        stmt.bind((1, key)).map_err(|e| e.to_string())?;
        if stmt.next().map_err(|e| e.to_string())? == sqlite::State::Row {
            let val: String = stmt.read(0).map_err(|e| e.to_string())?;
            Ok(Some(val))
        } else {
            Ok(None)
        }
    }

    pub fn set_config(&self, key: &str, value: &str) -> Result<(), String> {
        let conn = self.conn.lock().map_err(|e| e.to_string())?;
        let mut stmt = conn
            .prepare("INSERT OR REPLACE INTO config (key, value) VALUES (?, ?)")
            .map_err(|e| e.to_string())?;
        stmt.bind((1, key)).map_err(|e| e.to_string())?;
        stmt.bind((2, value)).map_err(|e| e.to_string())?;
        stmt.next().map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn delete_config(&self, key: &str) -> Result<(), String> {
        let conn = self.conn.lock().map_err(|e| e.to_string())?;
        let mut stmt = conn
            .prepare("DELETE FROM config WHERE key = ?")
            .map_err(|e| e.to_string())?;
        stmt.bind((1, key)).map_err(|e| e.to_string())?;
        stmt.next().map_err(|e| e.to_string())?;
        Ok(())
    }

    pub async fn verify_admin_password(&self, password: &str) -> Result<bool, String> {
        let hash = self.get_config("admin_password_hash")?;
        match hash {
            Some(h) => {
                let p = password.to_string();
                tokio::task::spawn_blocking(move || bcrypt::verify(&p, &h).unwrap_or(false))
                    .await
                    .map_err(|e| e.to_string())
            }
            None => Ok(false),
        }
    }

    pub async fn set_admin_password(&self, new_password: &str) -> Result<(), String> {
        let p = new_password.to_string();
        let hash = tokio::task::spawn_blocking(move || {
            bcrypt::hash(&p, bcrypt::DEFAULT_COST).map_err(|e| e.to_string())
        })
        .await
        .map_err(|e| e.to_string())??;
        self.set_config("admin_password_hash", &hash)
    }

    pub fn create_session(&self) -> Result<String, String> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .subsec_nanos();
        let pid = std::process::id();
        let token = format!("{:x}{:x}{:x}", now, nanos, pid);

        let conn = self.conn.lock().map_err(|e| e.to_string())?;
        let mut stmt = conn
            .prepare("INSERT INTO admin_sessions (token, created_at) VALUES (?, ?)")
            .map_err(|e| e.to_string())?;
        stmt.bind((1, token.as_str())).map_err(|e| e.to_string())?;
        stmt.bind((2, now as i64)).map_err(|e| e.to_string())?;
        stmt.next().map_err(|e| e.to_string())?;
        Ok(token)
    }

    pub fn validate_session(&self, token: &str) -> Result<bool, String> {
        let conn = self.conn.lock().map_err(|e| e.to_string())?;
        let mut stmt = conn
            .prepare("SELECT 1 FROM admin_sessions WHERE token = ?")
            .map_err(|e| e.to_string())?;
        stmt.bind((1, token)).map_err(|e| e.to_string())?;
        Ok(stmt.next().map_err(|e| e.to_string())? == sqlite::State::Row)
    }

    pub fn delete_session(&self, token: &str) -> Result<(), String> {
        let conn = self.conn.lock().map_err(|e| e.to_string())?;
        let mut stmt = conn
            .prepare("DELETE FROM admin_sessions WHERE token = ?")
            .map_err(|e| e.to_string())?;
        stmt.bind((1, token)).map_err(|e| e.to_string())?;
        stmt.next().map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn get_blacklist(&self) -> Result<Vec<String>, String> {
        let conn = self.conn.lock().map_err(|e| e.to_string())?;
        let mut stmt = conn
            .prepare("SELECT domain FROM blacklist ORDER BY domain ASC")
            .map_err(|e| e.to_string())?;
        let mut list = Vec::new();
        while stmt.next().map_err(|e| e.to_string())? == sqlite::State::Row {
            let d: String = stmt.read(0).map_err(|e| e.to_string())?;
            list.push(d);
        }
        Ok(list)
    }

    pub fn add_blacklist(&self, domain: &str) -> Result<(), String> {
        let normalized = domain.trim().to_lowercase();
        if normalized.is_empty() {
            return Err("Domain cannot be empty".to_string());
        }
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();

        let conn = self.conn.lock().map_err(|e| e.to_string())?;
        let mut stmt = conn
            .prepare("INSERT OR IGNORE INTO blacklist (domain, created_at) VALUES (?, ?)")
            .map_err(|e| e.to_string())?;
        stmt.bind((1, normalized.as_str())).map_err(|e| e.to_string())?;
        stmt.bind((2, now as i64)).map_err(|e| e.to_string())?;
        stmt.next().map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn remove_blacklist(&self, domain: &str) -> Result<(), String> {
        let normalized = domain.trim().to_lowercase();
        let conn = self.conn.lock().map_err(|e| e.to_string())?;
        let mut stmt = conn
            .prepare("DELETE FROM blacklist WHERE domain = ?")
            .map_err(|e| e.to_string())?;
        stmt.bind((1, normalized.as_str())).map_err(|e| e.to_string())?;
        stmt.next().map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn check_url_blacklisted(&self, url: &str) -> Result<Option<String>, String> {
        let Some(host) = extract_domain(url) else {
            return Ok(None);
        };
        let list = self.get_blacklist()?;
        for domain in list {
            let d = domain.to_lowercase();
            if host == d || host.ends_with(&format!(".{d}")) {
                return Ok(Some(domain));
            }
        }
        Ok(None)
    }
}

pub fn extract_domain(url: &str) -> Option<String> {
    let trimmed = url.trim();
    let without_scheme = if let Some(rest) = trimmed.strip_prefix("https://") {
        rest
    } else if let Some(rest) = trimmed.strip_prefix("http://") {
        rest
    } else {
        trimmed
    };

    let host_part = without_scheme
        .split(['/', '?', '#', ':'])
        .next()?
        .trim()
        .to_lowercase();

    if host_part.is_empty() {
        None
    } else {
        Some(host_part)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_db_default_password_and_change() {
        let test_db_path = format!(
            "test_db_{}.db",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let db = Database::open(&test_db_path).expect("open db");

        assert!(db.verify_admin_password("admin").await.expect("verify admin"));
        assert!(!db.verify_admin_password("wrong").await.expect("verify wrong"));

        db.set_admin_password("new_secret_123").await.expect("change pass");
        assert!(!db.verify_admin_password("admin").await.expect("verify old"));
        assert!(db.verify_admin_password("new_secret_123").await.expect("verify new"));

        let token = db.create_session().expect("create session");
        assert!(db.validate_session(&token).expect("validate session"));
        db.delete_session(&token).expect("delete session");
        assert!(!db.validate_session(&token).expect("validate deleted session"));

        let _ = std::fs::remove_file(&test_db_path);
    }

    #[test]
    fn test_extract_domain() {
        assert_eq!(
            extract_domain("https://www.youtube.com/watch?v=123"),
            Some("www.youtube.com".to_string())
        );
        assert_eq!(
            extract_domain("http://youtu.be/abc?t=10"),
            Some("youtu.be".to_string())
        );
        assert_eq!(
            extract_domain("https://example.com:8080/path"),
            Some("example.com".to_string())
        );
        assert_eq!(extract_domain("invalid-url"), Some("invalid-url".to_string()));
    }

    #[test]
    fn test_blacklist_crud_and_check() {
        let test_db_path = format!(
            "test_db_{}.db",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let db = Database::open(&test_db_path).expect("open db");

        assert_eq!(db.get_blacklist().expect("list"), Vec::<String>::new());
        assert_eq!(
            db.check_url_blacklisted("https://www.youtube.com/watch?v=123")
                .expect("check"),
            None
        );

        db.add_blacklist("youtube.com").expect("add youtube");
        db.add_blacklist("youtu.be").expect("add youtu.be");

        assert_eq!(
            db.check_url_blacklisted("https://www.youtube.com/watch?v=123")
                .expect("check"),
            Some("youtube.com".to_string())
        );
        assert_eq!(
            db.check_url_blacklisted("https://m.youtube.com/watch?v=123")
                .expect("check"),
            Some("youtube.com".to_string())
        );
        assert_eq!(
            db.check_url_blacklisted("https://youtu.be/123")
                .expect("check"),
            Some("youtu.be".to_string())
        );
        assert_eq!(
            db.check_url_blacklisted("https://soundcloud.com/track")
                .expect("check"),
            None
        );

        db.remove_blacklist("youtube.com").expect("remove youtube");
        assert_eq!(
            db.check_url_blacklisted("https://www.youtube.com/watch?v=123")
                .expect("check"),
            None
        );
        assert_eq!(
            db.check_url_blacklisted("https://youtu.be/123")
                .expect("check"),
            Some("youtu.be".to_string())
        );

        let _ = std::fs::remove_file(&test_db_path);
    }
}
