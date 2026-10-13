#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn diagnostic_activation_is_exclusive_and_typed() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let session = DiagnosticSession::open(&path).unwrap();
        session.emit(Event::cache_hit(Route::StoredKeyring, Endpoint::Other));
        session.close();
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("CacheHit"));
        assert!(DiagnosticSession::open(&path).is_err());
    }

    #[test]
    fn default_path_is_silent() {
        let dir = tempdir().unwrap();
        assert!(!dir.path().join("events.jsonl").exists());
    }
}
