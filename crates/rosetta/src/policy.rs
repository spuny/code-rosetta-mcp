pub fn credential_directory(path: &str) -> bool {
    path.replace('\\', "/").split('/').any(|part| {
        matches!(
            part.to_ascii_lowercase().as_str(),
            ".ssh" | ".kube" | ".aws" | ".gnupg"
        )
    })
}

pub fn credential_path(path: &str) -> bool {
    let path = path.replace('\\', "/").to_ascii_lowercase();
    let name = path.rsplit('/').next().unwrap_or(&path);
    credential_directory(&path)
        || path == ".docker/config.json"
        || path.ends_with("/.docker/config.json")
        || (name.starts_with(".env")
            && !name.ends_with(".example")
            && !name.ends_with(".template")
            && !name.ends_with(".sample"))
        || ["id_rsa", "id_dsa", "id_ecdsa", "id_ed25519"]
            .iter()
            .any(|prefix| name.starts_with(prefix))
        || name.contains("kubeconfig")
        || name.contains(".tfstate")
        || matches!(
            name,
            ".git-credentials"
                | ".netrc"
                | ".npmrc"
                | ".pypirc"
                | "credentials"
                | "credentials.json"
        )
        || [".pem", ".key", ".p12", ".pfx"]
            .iter()
            .any(|suffix| name.ends_with(suffix))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conventional_credentials_are_withheld_at_any_depth() {
        for path in [
            ".ssh/config",
            ".ssh/id_ecdsa",
            "nested/.kube/config",
            ".aws/config",
            ".gnupg/private-keys-v1.d/key",
            ".git-credentials",
            "id_dsa",
            "id_ecdsa_sk",
            "nested/.docker/config.json",
            "TLS/client.KEY",
            "folder\\.kube\\config",
        ] {
            assert!(credential_path(path), "{path}");
        }
        for path in [
            "app.py",
            "README.md",
            ".env.example",
            ".env.template",
            "Dockerfile",
            "kubernetes/config.yaml",
            "not.docker/config.json",
        ] {
            assert!(!credential_path(path), "{path}");
        }
    }
}
