// This Source Code Form is subject to the terms of the GNU Affero General Public
// License, v. 3.0. If a copy of the AGPL was not distributed with this
// file, You can obtain one at https://gnu.org/licenses/agpl-3.0.html.

//! Invitation links delivered by SMTP.
//!
//! `SMTP_URL`, `SMTP_FROM` and `PUBLIC_BASE_URL` together turn sending on. With
//! any one of them unset the API only hands the token back and whoever created
//! the invitation copies the link themselves.
//!
//! Sending happens on the request path rather than in a worker: a relay the
//! operator mistyped has to fail the request that used it, not a log line
//! nobody reads.

use lettre::{
    AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor,
    message::{Mailbox, header::ContentType},
};

const SMTP_URL_VAR: &str = "SMTP_URL";
const SMTP_FROM_VAR: &str = "SMTP_FROM";
const PUBLIC_BASE_URL_VAR: &str = "PUBLIC_BASE_URL";

/// The query parameter the viewer reads an invitation token from.
const INVITE_QUERY_PARAM: &str = "invite";

/// Where invitation mail comes from and where its link points. `None` is a
/// deployment with no relay, which is every deployment by default.
#[derive(Clone, Debug, Default)]
pub struct EmailConfig(Option<Smtp>);

#[derive(Clone, Debug)]
struct Smtp {
    url: String,
    from: String,
    public_base_url: String,
}

/// What an invitation email says, beyond its link.
pub struct InvitationEmail<'a> {
    pub recipient: &'a str,
    pub token: &'a str,
    /// `workspace` or `project`.
    pub target: &'a str,
    pub role: &'a str,
}

/// Why this deployment will not take the recipient, decided before anything is
/// created.
#[derive(Debug)]
pub enum RecipientError {
    /// No relay is configured, so a request naming a recipient cannot be served.
    NotConfigured,
    /// The recipient is not an address.
    Malformed(String),
}

/// Why the message did not go out. The invitation exists by then, so this
/// travels in the reply rather than replacing it.
#[derive(Debug)]
pub struct SendError(pub String);

impl std::fmt::Display for SendError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl EmailConfig {
    pub fn from_env() -> Self {
        match (
            configured_var(SMTP_URL_VAR),
            configured_var(SMTP_FROM_VAR),
            configured_var(PUBLIC_BASE_URL_VAR),
        ) {
            (Some(url), Some(from), Some(public_base_url)) => Self(Some(Smtp {
                url,
                from,
                public_base_url,
            })),
            _ => Self(None),
        }
    }

    pub fn new(
        url: impl Into<String>,
        from: impl Into<String>,
        public_base_url: impl Into<String>,
    ) -> Self {
        Self(Some(Smtp {
            url: url.into(),
            from: from.into(),
            public_base_url: public_base_url.into(),
        }))
    }

    pub fn is_configured(&self) -> bool {
        self.0.is_some()
    }

    /// Refuses a recipient this deployment cannot mail. Callers run this before
    /// the invitation row exists, so a refused request leaves nothing behind.
    pub fn check_recipient(&self, address: &str) -> Result<(), RecipientError> {
        if self.0.is_none() {
            return Err(RecipientError::NotConfigured);
        }
        address
            .parse::<Mailbox>()
            .map(|_| ())
            .map_err(|failure| RecipientError::Malformed(failure.to_string()))
    }

    pub async fn send_invitation(&self, invitation: InvitationEmail<'_>) -> Result<(), SendError> {
        let Some(smtp) = &self.0 else {
            return Err(SendError(format!("{SMTP_URL_VAR} is not set")));
        };
        let from = smtp.from.parse::<Mailbox>().map_err(|failure| {
            SendError(format!("{SMTP_FROM_VAR} is not an address: {failure}"))
        })?;
        let to = invitation
            .recipient
            .parse::<Mailbox>()
            .map_err(|failure| SendError(format!("recipient is not an address: {failure}")))?;

        let message = Message::builder()
            .from(from)
            .to(to)
            .subject(format!("You have been invited to a {}", invitation.target))
            .header(ContentType::TEXT_PLAIN)
            .body(body(smtp, &invitation))
            .map_err(|failure| SendError(failure.to_string()))?;

        let transport = AsyncSmtpTransport::<Tokio1Executor>::from_url(&smtp.url)
            .map_err(|failure| SendError(format!("{SMTP_URL_VAR} is unusable: {failure}")))?
            .build::<Tokio1Executor>();

        transport
            .send(message)
            .await
            .map(|_| ())
            .map_err(|failure| SendError(failure.to_string()))
    }
}

fn configured_var(name: &str) -> Option<String> {
    let value = std::env::var(name).ok()?;
    let value = value.trim().to_string();
    (!value.is_empty()).then_some(value)
}

fn invite_url(smtp: &Smtp, token: &str) -> String {
    format!(
        "{}/?{INVITE_QUERY_PARAM}={}",
        smtp.public_base_url.trim_end_matches('/'),
        urlencoding::encode(token)
    )
}

fn body(smtp: &Smtp, invitation: &InvitationEmail<'_>) -> String {
    format!(
        "You have been invited to a {} as {}.\n\nOpen this link to accept:\n\n{}\n",
        invitation.target,
        invitation.role,
        invite_url(smtp, invitation.token)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn configured() -> EmailConfig {
        EmailConfig::new(
            "smtp://127.0.0.1:2525",
            "ptolemy@example.com",
            "https://maps.example.com/",
        )
    }

    #[test]
    fn a_deployment_with_no_relay_refuses_a_recipient() {
        let error = EmailConfig::default()
            .check_recipient("someone@example.com")
            .unwrap_err();
        assert!(matches!(error, RecipientError::NotConfigured));
    }

    #[test]
    fn a_recipient_that_is_not_an_address_is_refused() {
        let error = configured().check_recipient("not an address").unwrap_err();
        assert!(matches!(error, RecipientError::Malformed(_)));
    }

    /// The trailing slash on the base url must not become a double slash: the
    /// viewer reads the token off the query string of its own index.
    #[test]
    fn the_link_carries_the_token_once() {
        let smtp = Smtp {
            url: "smtp://127.0.0.1:2525".into(),
            from: "ptolemy@example.com".into(),
            public_base_url: "https://maps.example.com/".into(),
        };
        assert_eq!(
            invite_url(&smtp, "abc-123"),
            "https://maps.example.com/?invite=abc-123"
        );
    }
}
