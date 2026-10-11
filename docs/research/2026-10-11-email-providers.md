# Transactional email provider comparison

Research pass run 2026-10-11, prompted by the discovery that Azure Communication
Services Email is being retired (onboarding closes 2026-10-23, end of service
2028-09-30). See `2026-10-11-prior-art.md` §0.1.

**Use case this is judged against:** verification codes and password resets only,
no marketing. Tens to low hundreds of emails per month initially. Signup is
*gated* on code delivery, so inbox placement is business-critical. Sending from
an own verified domain with SPF/DKIM/DMARC. Owner is in Denmark, so EU residency
is a genuine consideration. Integration is a plain HTTPS call from Rust.

Figures marked UNVERIFIED could not be confirmed against a vendor pricing page.

## A. Decision table

| | Free tier | Cheapest paid | @10k/mo | Deliverability evidence | Vendor risk | Time to first email | Rust story | EU residency |
|---|---|---|---|---|---|---|---|---|
| **Postmark** | 100/mo, perpetual | **$15/mo** Basic, 10k incl. | **$15** | #1 in both studies. ETT'23 **93.8%**; Mailtrap'25/26 **83.3%** — Gmail 100%, Outlook 100%. Txn/bulk on **separate IP ranges** | Changelog 24('21)→1('24)→9('26 YTD); prices **cut** 2025; ex-staff launched rival Oct'26 | Manual review of every account; no sandbox | `postmark` 2.0.1 community, 232k DL | **None** — "no plans to add servers in the EU" |
| **Amazon SES** | 3,000/mo **12 months only** | **$0.10/1,000** à la carte | **$1.00** | 77.1% inbox / 20.0% spam. Outlook 100%, Hotmail 100% — but **Gmail 4yr+ 22.2%**, Workspace 33.3% | Lowest; first-party since 2011. Dormant accts auto-default to paid Essentials from 2026-07-21 | **Sandbox**: 200/day, verified recipients only, ~24h review, per-region | `aws-sdk-sesv2` **official**, 3.83M DL | eu-north-1 Stockholm + **Sovereign Cloud** `eusc-de-east-1` |
| **Resend** | 3,000/mo + 100/day cap, perpetual | $20/mo Pro, 50k | **$20** | **None exists.** DNS-proven SES reseller → proxy 77.1% | a16z Series A Dec'24, **no Series B 18mo**; 2024 all-user metadata breach; abuse-control bypass | **Lowest** — verify domain, send | `resend-rs` **official**, 777k DL | Sending region only; "**all account data…stored in the US**" |
| **Scaleway TEM** | 300/mo, perpetual, PAYG | €0.25/1,000, no sub | **€2.43** | **None exists.** 41-day MS rate-limit Feb–Apr'26; Spamhaus 2026-09-30; pool = one /24 + **two /32s** | **12mo TEM changelog silence** while repo ships monthly; **no SLA** on entry plan | 48h domain verification | **No crate**; only Scaleway crate is GPL-3.0 | **Only fully-EU stack** (fr-par) |
| **Mailgun** | 100/**day**, perpetual | **$15/mo** Basic, 10k incl. | **$15** | 71.4% inbox / **23.8% spam** (worst). Hotmail 40%, Yahoo 33%, Zoho 0%. Queue-separated, not IP-separated | 4 owners/14yr; **"Email" deleted as Sinch segment** 2024-01-01; Flex killed | Sandbox = 5 authorized recipients | `mailgun-rs` community, 62k DL | `api.eu.` region-bound; free-tier gate UNVERIFIED |
| **MailerSend** | 500/mo, perpetual | $7.00/mo Hobby, 5k | ~**$14.50** UNVERIFIED (derived) | ETT'23 86.8% (round-2 dip 75%). Nothing since | Free tier **12k→3k→500 = −96%**, both cuts silent | **Sandbox: 2 recipients** until approved; opaque auto-rejections | **No crate exists** | EU data centres, **US legal entity** (NY) |
| **Brevo** | 300/**day** (~9k/mo), perpetual | $9/mo Starter, from 5k | **UNVERIFIED** — selector is in-app | ETT'23 **79.8% "below average"**. Marketing+txn **share IPs** by own admission | **€500M LBO** Dec'25; pivoting to CRM/CDP/AI; profitable | Opaque approval gate | No official SDK; `brevo` crate has **no license file** | EU-only (OVH FR/DE + GCP Belgium) |
| **SendGrid** | **Killed 2025-05-27**; 60-day trial | $19.95/mo Essentials, 50k | **$19.95** | **Worst.** 61.0% inbox, **20.9% MISSING**, **Outlook 0%, Hotmail 0%**. Own docs: shared IPs "**poorly suited**" to password resets | Free-tier rug-pull **deleted contacts**; absorbed into "Twilio Email"; active phishing abuse | UNVERIFIED | `sendgrid` community, 2.64M DL | **Pro/Premier only** + EU subuser + dedicated EU IP |

Sources: [Postmark](https://postmarkapp.com/pricing), [SES pricing](https://aws.amazon.com/ses/pricing/),
[SES FAQ](https://aws.amazon.com/ses/faqs/), [Resend](https://resend.com/pricing),
[Scaleway](https://www.scaleway.com/en/pricing/managed-services/), [Mailgun](https://www.mailgun.com/pricing/),
[MailerSend](https://www.mailersend.com/pricing), [Brevo](https://www.brevo.com/pricing/),
[SendGrid](https://www.twilio.com/en-us/products/email-api/pricing).

## B. Free-tier detail — the axis that decides this at low volume

| Provider | Allowance | Perpetual? | The catch |
|---|---|---|---|
| **Resend** | 3,000/mo | Yes | Hard **100/day** cap; 30-day log retention; 3 domains. No branding, custom domain OK. |
| **Brevo** | 300/day ≈ 9,000/mo | Yes | **"Sent with Brevo" sticker always** — on a verification email. Txn overflow: 1,000 queued, "beyond this queue are not delivered". |
| **Mailgun** | 100/day ≈ 3,000/mo | Yes | **1-day log retention** — you cannot debug yesterday. 1 domain, 1 user. |
| **MailerSend** | 500/mo | Yes | **Sandbox: 2 recipients total** until approved; approval is opaque and has rejected legitimate senders. |
| **Scaleway TEM** | 300/mo | Yes | **No SLA** on entry plan. Auto-locks domain at ~7% rejections/24h — **≈1 bounce at this volume**. |
| **Postmark** | 100/mo | Yes — "never expires or runs out" | Hard stop at 100, no overages. Manual account review before first send. **No branding, no sandbox.** |
| **Amazon SES** | 3,000/mo | **No — 12 months only** | Sandbox until approved; per-region. |
| **SendGrid** | 100/day | **No — 60-day trial** | Then sending pauses; **>100 contacts deleted, unrecoverable**. |

## C. Amazon SES sandbox and free tier

**Sandbox restrictions** ([docs](https://docs.aws.amazon.com/ses/latest/dg/request-production-access.html),
[quotas](https://docs.aws.amazon.com/ses/latest/dg/quotas.html)):

- Send only to **verified addresses/domains** or the mailbox simulator — this alone blocks real signups.
- **200 emails per 24 hours**, **1 email/second**.
- Suppression-management API calls disabled.
- **Sandbox status is per Region** — adding `eu-north-1` later means repeating the exit.

**Exit request:** Console → Account dashboard → "Request production access". Requires a
Marketing/Transactional radio, website URL, contact emails, and an acknowledgement that you
only send to people who requested it and have bounce/complaint handling. Not editable once
submitted. Verifying the domain first is documented as speeding approval.
**Initial response within 24 hours.**

**Post-sandbox quotas** "vary based on your specific use case" — AWS publishes no fixed number.
The commonly-cited 50,000/day + 14/sec is UNVERIFIED.

**Did SES remove the perpetual free tier? Yes — confirmed.** The old 62,000/month-from-EC2
allowance is absent from the current pricing page. Current allowance is
"up to **3,000 message charges for free each month for the first 12 months**"
([FAQ](https://aws.amazon.com/ses/faqs/)), plus up to $200 Free Tier credits for 6 months
([pricing](https://aws.amazon.com/ses/pricing/)). Exact withdrawal date of the old tier UNVERIFIED.

## D. Ranked recommendation

**1. Postmark — $15/mo Basic, free at 100/mo during early development.**
Signup is gated on code delivery, so placement dominates every other axis, and Postmark is the
only provider that wins it on *independent* evidence — #1 in both studies, run years apart by
unrelated parties (EmailToolTester 2023: 93.8%; Mailtrap 2025/26: 83.3%, with 100% at Gmail,
Google Workspace and Outlook). Mailtrap ranked itself #2 and had every incentive not to crown a
rival, which strengthens the result. The mechanism is structural rather than marketing:
transactional and bulk traffic are separated "including IP ranges", and every new account is
manually reviewed, which is what keeps the shared pool clean. Their stated position — "a pristine
shared IP pool will have the best results", "no need to go through a warming-up period" — is
precisely tuned to a brand-new domain sending tens of emails a month. Prices were *cut* in 2025.
**Trade-off accepted: no EU data residency at all**, and no plans for it. For OTP payloads (an
address and a short-lived code, 45-day retention) under a DPA with SCCs this is defensible, but
if EU residency is a hard requirement Postmark is eliminated immediately. Secondary costs:
community-only Rust crate, no idempotency.

**2. Amazon SES in `eu-north-1` (Stockholm) — ~$1/mo at 10k.**
The right pick if EU residency is non-negotiable. Cheapest by an order of magnitude, no
acquisition or sunset risk, the highest-download official Rust SDK here, and a genuine
sovereignty escape hatch in the AWS European Sovereign Cloud (`eusc-de-east-1`, GA Jan 2026,
EU-resident staff, German legal entities). **Trade-offs accepted:** the sandbox means no real
user can be emailed until a human approves, and it is per-region; and the one hard SES-specific
signal is **22.2% placement at mature Gmail** in the no-warm-up test — the inverse of Postmark's
profile, and for a consumer messenger that is the material risk.

**3. Resend — free at 3,000/mo, $20/mo Pro.**
Best developer experience by a distance: the only first-party Rust SDK, and **the only provider
of the eight with real idempotency** (`Idempotency-Key`, 24h dedup window). For retry-safe
sending from a service Kubernetes may restart mid-request, that is a correctness advantage
nothing else offers. **Trade-offs accepted:** you are buying SES with a nicer wrapper — DNS
proves it (`send.resend.com` SPF includes `amazonses.com`) — so you inherit SES's placement
profile plus thinner abuse policing; no EU residency whatsoever; and unproven governance
(2024 breach exposing all users' metadata, documented abuse-control bypass, no Series B in 18+ months).

**Overall pick: Postmark**, unless EU residency is a hard requirement, in which case **SES `eu-north-1`**.

**Explicitly ruled out: SendGrid** — on Twilio's own documentation (shared IPs "poorly suited"
to password resets; "Twilio can't move your account to another IP pool"), two independent tests
three years apart both finding Microsoft-specific failure (0% Outlook, 0% Hotmail, 20.9%
missing), and a free-tier removal that irrecoverably deleted contacts.

## E. What would trigger a switch

| Trigger | Signal to watch |
|---|---|
| **Own placement turns** | The only signal that truly matters. Verification-completion rate segmented by recipient domain, plus "didn't get the code" support contacts. Watch **Microsoft** first — industry-wide the hardest target at 77.4% (Validity 2026). |
| **EU residency becomes contractual** | A customer, DPIA, or Danish procurement requirement. Postmark is disqualified by its own stated policy. **Most likely trigger in practice.** |
| **ActiveCampaign changes Postmark's shape** | Any relaxation of manual account review or message-stream IP separation — *that separation is the entire thesis*. Also a price rise reversing the 2025 cut. |
| **Volume crosses ~50k/mo** | SES's $0.10/1,000 and Postmark's $1.80/1,000 overage diverge sharply. |
| **Idempotency or signed webhooks needed** | If a Redis dedup layer is being hand-rolled around their absence, Resend's `Idempotency-Key` stops being a nice-to-have. |

## F. What the provider abstraction must cover

Two traits, not one — the send path and the webhook path have independent provider semantics,
and SES does not even use HTTP for the latter.

```rust
#[async_trait]
pub trait EmailSender: Send + Sync {
    async fn send(
        &self,
        msg: &Message,
        idempotency_key: Option<&str>,
    ) -> Result<SentMessage, SendError>;   // SentMessage { provider_message_id, region }

    fn constraints(&self) -> &Constraints; // subject min/max, max recipients, max size
    fn supports_idempotency(&self) -> bool;
}

pub trait WebhookVerifier: Send + Sync {
    fn verify(&self, raw_body: &[u8], headers: &HeaderMap) -> Result<(), VerifyError>;
    fn parse(&self, raw_body: &[u8]) -> Result<Vec<DeliveryEvent>, ParseError>;
}
```

What it must *not* hide: which region the message went to, the provider's message ID (needed to
correlate webhooks back to a signup attempt), and whether open-tracking was disabled. On that
last point — "We recommend disabling open rates for transactional email, to maximize inbox
placement" ([Resend docs](https://resend.com/docs/dashboard/emails/open-tracking)). A tracking
pixel on a verification code buys nothing and costs placement, so `tracking: TrackingMode`
belongs on the canonical `Message` and defaults to `Off`.

**Mailgun is the one that will fight this abstraction** — the only non-JSON API in the set, so
its adapter needs a multipart code path nothing else uses.

## G. Evidence gaps

The largest gap is structural: **no measured inbox-placement data exists for Resend or Scaleway
TEM anywhere** — verified absent from EmailToolTester, Mailtrap, GlockApps public reports,
Validity 2025/2026, and academic literature. Resend's figure above is a proxy via SES, justified
by DNS but not a measurement. MailerSend has no post-2023 data. Mailgun and SES shared-pool abuse
has no evidence either way — treat as an evidence gap, not a clean bill of health.

Unreliable sources encountered, flagged so they are not rediscovered: pikvue.com,
emaildeliverabilityreport.com (Seychelles-registered affiliate operation with silently-reset
datasets), mailreach.co's per-ESP table (attributed to a Validity report containing no ESP data),
sendbridge.com, warmy.io/blog, toolradar.com, sequenzy.com, apiscout.dev, and Zenodo DOIs
polluted with affiliate SEO spam — a DOI confers no credibility here. Kickbox's report is an
opinion survey of US marketers with zero placement measurements.

**The most useful conclusion:** stop relying on anyone's benchmark, including the ones above.
Instrument the signup funnel from day one — log the provider message ID, capture the
bounce/complaint webhook, and track verification-completion rate segmented by recipient domain.
Within a few hundred signups that is better data for this specific domain, content and recipient
mix than any seed-list study, which is precisely why EmailToolTester shut its own study down.
The `EmailSender` trait exists so that acting on that data is a config change, not a refactor.
