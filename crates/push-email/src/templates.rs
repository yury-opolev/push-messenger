//! The email templates of auth-svc's `email.send` events (spec §3.1): plain text and a
//! minimal HTML version of each, with every parameter escaped.

use serde_json::Value;

/// A rendered email: what the sender sends.
#[derive(Clone, PartialEq, Eq)]
pub struct Rendered {
    pub subject: String,
    pub text: String,
    pub html: String,
}

impl std::fmt::Debug for Rendered {
    /// Only the subject: the bodies carry sign-in links.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Rendered")
            .field("subject", &self.subject)
            .finish_non_exhaustive()
    }
}

/// Why a template could not be rendered. Never worth retrying.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TemplateError {
    #[error("unknown email template {0:?}")]
    UnknownTemplate(String),
    #[error("the email template parameter {0} is missing or invalid")]
    BadParam(&'static str),
}

/// The templates, with the product name they sign with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Templates {
    product: String,
}

impl Templates {
    /// `product` names the service in subjects and bodies, `Push Messenger` by default.
    pub fn new(product: impl Into<String>) -> Self {
        Self {
            product: product.into(),
        }
    }

    /// Renders `template` with `params`:
    ///
    /// | template | params |
    /// |---|---|
    /// | `verify_email` | `link`, `handle`, `expires_in_minutes` |
    /// | `account_exists` | `handle` |
    /// | `reset_password` | `link`, `expires_in_minutes` |
    ///
    /// Links must be `https://` or `http://` URLs.
    pub fn render(&self, template: &str, params: &Value) -> Result<Rendered, TemplateError> {
        let product = &self.product;
        match template {
            "verify_email" => {
                let link = link(params)?;
                let handle = text(params, "handle")?;
                let minutes = minutes(params)?;
                Ok(self.page(
                    format!("Verify your email address for {product}"),
                    &[
                        format!("Hi @{handle},"),
                        format!("Open this link to verify your email address for {product}:"),
                    ],
                    Some(link),
                    &[
                        format!("The link works for {minutes} minutes."),
                        "If you did not sign up, ignore this email.".to_owned(),
                    ],
                ))
            }
            "account_exists" => {
                let handle = text(params, "handle")?;
                Ok(self.page(
                    format!("You already have a {product} account"),
                    &[
                        format!(
                            "Someone, perhaps you, tried to sign up for {product} with this \
                             email address. It already has an account: @{handle}."
                        ),
                        "If you forgot your password, reset it from the app. If it was not \
                         you, ignore this email."
                            .to_owned(),
                    ],
                    None,
                    &[],
                ))
            }
            "reset_password" => {
                let link = link(params)?;
                let minutes = minutes(params)?;
                Ok(self.page(
                    format!("Reset your {product} password"),
                    &[format!(
                        "Open this link to choose a new password for {product}:"
                    )],
                    Some(link),
                    &[
                        format!("The link works for {minutes} minutes and only once."),
                        "If you did not ask for it, ignore this email: your password stays as \
                         it is."
                            .to_owned(),
                    ],
                ))
            }
            other => Err(TemplateError::UnknownTemplate(other.to_owned())),
        }
    }

    /// Paragraphs `before`, an optional link, and paragraphs `after`, as text and as HTML.
    fn page(
        &self,
        subject: String,
        before: &[String],
        link: Option<&str>,
        after: &[String],
    ) -> Rendered {
        let mut text = Vec::new();
        let mut html = Vec::new();
        for paragraph in before {
            text.push(paragraph.clone());
            html.push(format!("<p>{}</p>", escape(paragraph)));
        }
        if let Some(link) = link {
            text.push(link.to_owned());
            html.push(format!(
                "<p><a href=\"{link}\">{link}</a></p>",
                link = escape(link)
            ));
        }
        for paragraph in after {
            text.push(paragraph.clone());
            html.push(format!("<p>{}</p>", escape(paragraph)));
        }
        text.push(format!("— {}", self.product));
        html.push(format!("<p>— {}</p>", escape(&self.product)));
        Rendered {
            html: format!(
                "<!doctype html><html><head><meta charset=\"utf-8\"><title>{}</title></head>\
                 <body>{}</body></html>",
                escape(&subject),
                html.join("")
            ),
            text: text.join("\n\n") + "\n",
            subject,
        }
    }
}

impl Default for Templates {
    fn default() -> Self {
        Self::new("Push Messenger")
    }
}

fn text<'a>(params: &'a Value, name: &'static str) -> Result<&'a str, TemplateError> {
    params[name]
        .as_str()
        .filter(|value| !value.is_empty())
        .ok_or(TemplateError::BadParam(name))
}

fn link(params: &Value) -> Result<&str, TemplateError> {
    let link = text(params, "link")?;
    if (link.starts_with("https://") || link.starts_with("http://"))
        && !link.contains(char::is_whitespace)
    {
        Ok(link)
    } else {
        Err(TemplateError::BadParam("link"))
    }
}

fn minutes(params: &Value) -> Result<u64, TemplateError> {
    params["expires_in_minutes"]
        .as_u64()
        .ok_or(TemplateError::BadParam("expires_in_minutes"))
}

fn escape(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&#39;"),
            other => escaped.push(other),
        }
    }
    escaped
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const LINK: &str = "https://api.example.com/v1/auth/verify?token=abc.def";

    #[test]
    fn verify_email_carries_the_link_the_handle_and_the_lifetime() {
        let rendered = Templates::default()
            .render(
                "verify_email",
                &json!({"link": LINK, "handle": "alice", "expires_in_minutes": 30}),
            )
            .unwrap();
        assert_eq!(
            rendered.subject,
            "Verify your email address for Push Messenger"
        );
        assert!(rendered.text.starts_with("Hi @alice,\n\n"));
        assert!(rendered.text.contains(&format!("\n\n{LINK}\n\n")));
        assert!(rendered.text.contains("The link works for 30 minutes."));
        assert!(
            rendered
                .html
                .contains(&format!("<a href=\"{LINK}\">{LINK}</a>"))
        );
    }

    #[test]
    fn every_template_renders_and_names_the_product() {
        let templates = Templates::new("Acme Chat");
        for (template, params) in [
            (
                "verify_email",
                json!({"link": LINK, "handle": "alice", "expires_in_minutes": 30}),
            ),
            ("account_exists", json!({"handle": "alice"})),
            (
                "reset_password",
                json!({"link": LINK, "expires_in_minutes": 30}),
            ),
        ] {
            let rendered = templates.render(template, &params).unwrap();
            assert!(rendered.subject.contains("Acme Chat"), "{template}");
            assert!(rendered.text.ends_with("— Acme Chat\n"), "{template}");
            assert!(rendered.html.contains("<p>— Acme Chat</p>"), "{template}");
        }
    }

    #[test]
    fn parameters_are_escaped_in_html() {
        let rendered = Templates::new("A&B")
            .render("account_exists", &json!({"handle": "<script>"}))
            .unwrap();
        assert!(rendered.html.contains("@&lt;script&gt;"));
        assert!(!rendered.html.contains("<script>"));
        assert!(rendered.html.contains("A&amp;B"));
    }

    #[test]
    fn unknown_templates_and_bad_parameters_are_refused() {
        let templates = Templates::default();
        assert_eq!(
            templates.render("welcome", &json!({})),
            Err(TemplateError::UnknownTemplate("welcome".to_owned()))
        );
        for (params, missing) in [
            (json!({"handle": "a", "expires_in_minutes": 30}), "link"),
            (
                json!({"link": "javascript:alert(1)", "handle": "a", "expires_in_minutes": 30}),
                "link",
            ),
            (
                json!({"link": "https://x.test/a b", "handle": "a", "expires_in_minutes": 30}),
                "link",
            ),
            (json!({"link": LINK, "expires_in_minutes": 30}), "handle"),
            (
                json!({"link": LINK, "handle": "a", "expires_in_minutes": "30"}),
                "expires_in_minutes",
            ),
        ] {
            assert_eq!(
                templates.render("verify_email", &params),
                Err(TemplateError::BadParam(missing)),
                "{params}"
            );
        }
    }

    #[test]
    fn debug_output_hides_the_bodies() {
        let rendered = Templates::default()
            .render(
                "reset_password",
                &json!({"link": LINK, "expires_in_minutes": 30}),
            )
            .unwrap();
        let shown = format!("{rendered:?}");
        assert!(!shown.contains("token"), "{shown}");
        assert!(
            shown.contains("Reset your Push Messenger password"),
            "{shown}"
        );
    }
}
