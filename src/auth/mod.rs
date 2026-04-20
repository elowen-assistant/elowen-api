//! Authentication provider, actor, and authorization types.

use anyhow::{Context, anyhow};
use argon2::{
    Argon2,
    password_hash::{Error as PasswordHashError, PasswordHash, PasswordVerifier},
};
use serde::{Deserialize, Serialize};
use std::{fs, path::PathBuf};

use crate::error::AppError;

const LEGACY_ADMIN_USERNAME: &str = "legacy-admin";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AuthMode {
    Disabled,
    LegacySharedPassword,
    LocalAccounts,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AuthRole {
    Viewer,
    Operator,
    Admin,
}

impl AuthRole {
    pub(crate) fn allows(&self, required: &Self) -> bool {
        self.rank() >= required.rank()
    }

    pub(crate) fn permissions(&self) -> Vec<AuthPermission> {
        match self {
            Self::Viewer => vec![AuthPermission::View],
            Self::Operator => vec![AuthPermission::View, AuthPermission::Operate],
            Self::Admin => vec![
                AuthPermission::View,
                AuthPermission::Operate,
                AuthPermission::Admin,
            ],
        }
    }

    fn rank(&self) -> u8 {
        match self {
            Self::Viewer => 1,
            Self::Operator => 2,
            Self::Admin => 3,
        }
    }
}

impl TryFrom<&str> for AuthRole {
    type Error = anyhow::Error;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        match value.trim().to_ascii_lowercase().as_str() {
            "viewer" => Ok(Self::Viewer),
            "operator" => Ok(Self::Operator),
            "admin" => Ok(Self::Admin),
            other => Err(anyhow!("unsupported auth role `{other}`")),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AuthPermission {
    View,
    Operate,
    Admin,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct SessionActor {
    pub(crate) username: String,
    pub(crate) display_name: String,
    pub(crate) role: AuthRole,
}

impl SessionActor {
    pub(crate) fn permissions(&self) -> Vec<AuthPermission> {
        self.role.permissions()
    }
}

#[derive(Clone)]
pub(crate) enum AuthProvider {
    Disabled,
    LegacySharedPassword(LegacySharedPasswordProvider),
    LocalAccounts(LocalAccountsProvider),
}

impl AuthProvider {
    pub(crate) fn from_config(
        auth_config_path: Option<String>,
        password: Option<String>,
        operator_label: String,
    ) -> anyhow::Result<Self> {
        if let Some(path) = auth_config_path
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
        {
            return Ok(Self::LocalAccounts(LocalAccountsProvider {
                path: PathBuf::from(path),
            }));
        }

        if let Some(password) = password.filter(|value| !value.is_empty()) {
            return Ok(Self::LegacySharedPassword(LegacySharedPasswordProvider {
                password,
                display_name: operator_label,
            }));
        }

        Ok(Self::Disabled)
    }

    pub(crate) fn enabled(&self) -> bool {
        !matches!(self, Self::Disabled)
    }

    pub(crate) fn mode(&self) -> AuthMode {
        match self {
            Self::Disabled => AuthMode::Disabled,
            Self::LegacySharedPassword(_) => AuthMode::LegacySharedPassword,
            Self::LocalAccounts(_) => AuthMode::LocalAccounts,
        }
    }

    pub(crate) fn authenticate(
        &self,
        username: Option<&str>,
        password: &str,
    ) -> Result<SessionActor, AppError> {
        match self {
            Self::Disabled => Err(AppError::conflict(anyhow!(
                "web UI authentication is disabled"
            ))),
            Self::LegacySharedPassword(provider) => provider.authenticate(password),
            Self::LocalAccounts(provider) => provider.authenticate(username, password),
        }
    }

    pub(crate) fn resolve_actor(&self, username: &str) -> Result<Option<SessionActor>, AppError> {
        match self {
            Self::Disabled => Ok(None),
            Self::LegacySharedPassword(provider) => Ok(provider.resolve_actor(username)),
            Self::LocalAccounts(provider) => provider.resolve_actor(username),
        }
    }
}

#[derive(Clone)]
pub(crate) struct LegacySharedPasswordProvider {
    password: String,
    display_name: String,
}

impl LegacySharedPasswordProvider {
    fn authenticate(&self, password: &str) -> Result<SessionActor, AppError> {
        if password != self.password {
            return Err(AppError::unauthorized(anyhow!("invalid password")));
        }

        Ok(self.actor())
    }

    fn resolve_actor(&self, username: &str) -> Option<SessionActor> {
        (username == LEGACY_ADMIN_USERNAME).then(|| self.actor())
    }

    fn actor(&self) -> SessionActor {
        SessionActor {
            username: LEGACY_ADMIN_USERNAME.to_string(),
            display_name: self.display_name.clone(),
            role: AuthRole::Admin,
        }
    }
}

#[derive(Clone)]
pub(crate) struct LocalAccountsProvider {
    path: PathBuf,
}

impl LocalAccountsProvider {
    fn authenticate(
        &self,
        username: Option<&str>,
        password: &str,
    ) -> Result<SessionActor, AppError> {
        let username = username
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| AppError::bad_request(anyhow!("username is required")))?;
        let accounts = self.load_accounts()?;
        let Some(account) = accounts
            .into_iter()
            .find(|account| account.username == username)
        else {
            return Err(AppError::unauthorized(anyhow!(
                "invalid username or password"
            )));
        };

        if account.disabled {
            return Err(AppError::unauthorized(anyhow!(
                "invalid username or password"
            )));
        }

        let password_hash = PasswordHash::new(&account.password_hash).map_err(|error| {
            AppError::from(anyhow!(
                "invalid password hash for account `{}`: {error}",
                account.username
            ))
        })?;

        match Argon2::default().verify_password(password.as_bytes(), &password_hash) {
            Ok(_) => Ok(account.actor()),
            Err(PasswordHashError::Password) => Err(AppError::unauthorized(anyhow!(
                "invalid username or password"
            ))),
            Err(error) => Err(AppError::from(anyhow!(
                "failed to verify password for account `{}`: {error}",
                account.username
            ))),
        }
    }

    fn resolve_actor(&self, username: &str) -> Result<Option<SessionActor>, AppError> {
        let accounts = self.load_accounts()?;
        Ok(accounts
            .into_iter()
            .find(|account| account.username == username && !account.disabled)
            .map(|account| account.actor()))
    }

    fn load_accounts(&self) -> Result<Vec<LocalAccount>, AppError> {
        let raw = fs::read_to_string(&self.path).with_context(|| {
            format!(
                "failed to read UI auth config from `{}`",
                self.path.display()
            )
        })?;
        let parsed: LocalAccountsFile = toml::from_str(&raw).with_context(|| {
            format!(
                "failed to parse UI auth config from `{}`",
                self.path.display()
            )
        })?;

        if parsed.accounts.is_empty() {
            return Err(AppError::from(anyhow!(
                "UI auth config `{}` does not define any accounts",
                self.path.display()
            )));
        }

        let mut seen = std::collections::HashSet::new();
        let mut accounts = Vec::with_capacity(parsed.accounts.len());
        for account in parsed.accounts {
            let username = account.username.trim().to_string();
            if username.is_empty() {
                return Err(AppError::from(anyhow!(
                    "UI auth config `{}` contains an empty username",
                    self.path.display()
                )));
            }
            if !seen.insert(username.clone()) {
                return Err(AppError::from(anyhow!(
                    "UI auth config `{}` contains duplicate username `{username}`",
                    self.path.display()
                )));
            }

            let display_name = account
                .display_name
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| username.clone());

            accounts.push(LocalAccount {
                username,
                display_name,
                role: account.role,
                password_hash: account.password_hash,
                disabled: account.disabled,
            });
        }

        Ok(accounts)
    }
}

#[derive(Debug, Deserialize)]
struct LocalAccountsFile {
    #[serde(default)]
    accounts: Vec<LocalAccountConfig>,
}

#[derive(Debug, Deserialize)]
struct LocalAccountConfig {
    username: String,
    display_name: Option<String>,
    role: AuthRole,
    password_hash: String,
    #[serde(default)]
    disabled: bool,
}

#[derive(Debug)]
struct LocalAccount {
    username: String,
    display_name: String,
    role: AuthRole,
    password_hash: String,
    disabled: bool,
}

impl LocalAccount {
    fn actor(self) -> SessionActor {
        SessionActor {
            username: self.username,
            display_name: self.display_name,
            role: self.role,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{AuthMode, AuthProvider, AuthRole};
    use argon2::{
        Argon2,
        password_hash::{PasswordHasher, SaltString},
    };
    use std::{
        fs,
        time::{SystemTime, UNIX_EPOCH},
    };

    #[test]
    fn auth_role_permissions_are_ordered() {
        assert!(AuthRole::Admin.allows(&AuthRole::Operator));
        assert!(AuthRole::Operator.allows(&AuthRole::Viewer));
        assert!(!AuthRole::Viewer.allows(&AuthRole::Operator));
    }

    #[test]
    fn local_account_provider_authenticates_and_resolves_accounts() {
        let path = std::env::temp_dir().join(format!(
            "elowen-auth-{}.toml",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("duration")
                .as_nanos()
        ));
        let salt = SaltString::encode_b64(b"0123456789abcdef").expect("valid static salt");
        let password_hash = Argon2::default()
            .hash_password("slice32-admin".as_bytes(), &salt)
            .expect("hash")
            .to_string();
        let body = format!(
            "[[accounts]]\nusername = \"admin\"\ndisplay_name = \"Admin User\"\nrole = \"admin\"\npassword_hash = \"{password_hash}\"\n"
        );
        fs::write(&path, body).expect("write config");

        let provider = AuthProvider::from_config(
            Some(path.display().to_string()),
            None,
            "Ignored".to_string(),
        )
        .expect("provider");
        assert_eq!(provider.mode(), AuthMode::LocalAccounts);

        let actor = provider
            .authenticate(Some("admin"), "slice32-admin")
            .expect("authenticate");
        assert_eq!(actor.username, "admin");
        assert_eq!(actor.display_name, "Admin User");
        assert_eq!(actor.role, AuthRole::Admin);

        let resolved = provider
            .resolve_actor("admin")
            .expect("resolve actor")
            .expect("actor present");
        assert_eq!(resolved.username, "admin");

        fs::remove_file(path).expect("cleanup");
    }
}
