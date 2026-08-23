//! The fixed retrieval corpus + labeled query set (feature gap G4).
//!
//! A **frozen** support knowledge base for a fictional company (Northwind
//! Robotics) plus a hand-labeled query set. Freezing both is the whole point: if
//! the corpus or the labels move, the recall/MRR numbers stop being comparable
//! across commits and the regression suite becomes a random-number generator.
//! Treat edits here the way you'd treat editing a golden file — deliberate, and
//! with the new baseline numbers re-measured and written into
//! [`crate::retrieval`]'s thresholds.
//!
//! ## Why these documents
//!
//! The corpus is built to be **hard enough to regress**. The first draft of this
//! file was 13 unrelated documents and scored a perfect recall@3 with *every*
//! degradation still passing — it detected nothing. So it is deliberately full of
//! *near misses*: documents that share most of a query's vocabulary while only
//! one of them answers it.
//!
//! | Answer | Competing near-duplicates | Shared vocabulary |
//! | --- | --- | --- |
//! | `policies/returns.md` | `policies/exchanges.md`, `policies/cancellations.md`, `billing/refunds.md` | return, 17-day window, delivery date, prepaid label, restocking |
//! | `billing/refunds.md` | `policies/cancellations.md`, `billing/taxes.md` | refund, original payment method, business days, tax collected |
//! | `policies/shipping.md` | `policies/international-shipping.md` | shipping, business days, carrier, freight appointment |
//! | `product/atlas-r7-specs.md` | `product/atlas-r5-specs.md`, `support/battery-care.md` | atlas, battery runtime, payload, IP rating, lidar |
//! | `product/charging-dock.md` | `support/battery-care.md` | pack, charge cycles, percent, temperature |
//! | `support/error-codes.md` | `support/diagnostics.md` | encoder ticks, fault, drive controller, degrees Celsius |
//! | `support/firmware-update.md` | `support/network-setup.md` | firmware image, download, docked, maintenance window |
//! | `policies/warranty.md` | `support/battery-care.md`, `product/end-effectors.md` | warranty term, 70 percent capacity, third-party effectors |
//!
//! Facts are also stated with **unusual, specific numbers** (a 17-day return
//! window, a 43-minute dock charge) so a retrieval hit is a real hit and not a
//! generic paragraph that happens to contain the query's words.
//!
//! Half the queries deliberately target a fact stated in a document's **second
//! or third** paragraph rather than its opening one. A query set that only ever
//! asks about a document's first paragraph measures topic matching, not fact
//! retrieval — and is blind to any regression that loses document tails.
//!
//! Several documents are longer than the chunker's 500-char cap on purpose, so
//! the ingest→chunk→store path actually chunks and a chunker regression (a
//! boundary that splits a fact away from its question's vocabulary) is
//! observable in the numbers.

use smooth_operator_ingestion::RawDocument;

/// One labeled query: the search text plus the document `source`s that actually
/// answer it.
///
/// Labels are **document sources**, not chunk ids — a chunker change reshuffles
/// chunk ids but must not change which document answers a question, so labeling
/// at the document level keeps the ground truth stable across chunker edits.
#[derive(Debug, Clone)]
pub struct LabeledQuery {
    /// The search text, phrased the way `knowledge_search`'s own schema tells the
    /// model to phrase it: key terms expected to appear in the answer.
    pub query: &'static str,
    /// The `source` of every document that answers this query. Non-empty.
    pub relevant: &'static [&'static str],
}

/// The frozen corpus: 20 documents, each with a unique `source` used as its
/// retrieval-ground-truth identity.
#[must_use]
pub fn corpus() -> Vec<RawDocument> {
    DOCS.iter()
        .map(|(source, title, body)| RawDocument::new(*source, *source, *body).with_title(*title))
        .collect()
}

/// The frozen labeled query set: 20 queries over the corpus.
#[must_use]
pub fn labeled_queries() -> &'static [LabeledQuery] {
    QUERIES
}

/// `(source, title, content)` for every corpus document.
const DOCS: &[(&str, &str, &str)] = &[
    (
        "policies/returns.md",
        "Return policy",
        "Northwind Robotics accepts returns within 17 days of the delivery date. The 17-day \
         return window starts the day the carrier marks the shipment delivered, not the day you \
         placed the order.\n\n\
         To open a return, sign in and choose Start a return from the order detail page. You will \
         receive a prepaid label by email. The unit must be returned in its original packaging \
         with the charging dock and both battery packs.\n\n\
         Units returned after the 17-day window are assessed a restocking fee of 15 percent. \
         Custom-configured fleet units and units with a registered serial transfer are final sale \
         and cannot be returned at all.",
    ),
    (
        "billing/refunds.md",
        "Refund processing",
        "Once a returned unit is received and inspected at the Columbus depot, the refund is \
         issued to the original payment method. Card refunds settle in 5 business days; ACH and \
         wire refunds settle in 7 to 10 business days.\n\n\
         Refunds are issued for the purchase price and any tax collected. Original expedited \
         shipping charges are not refunded. If the original payment method is closed, the refund \
         is issued as account credit and can be withdrawn by contacting billing support.",
    ),
    (
        "policies/shipping.md",
        "Domestic shipping",
        "Standard shipping inside the continental United States takes 5 to 7 business days from \
         the ship date. Expedited shipping takes 2 business days and is free on orders over 750 \
         dollars.\n\n\
         Orders placed after 2pm Eastern ship the following business day. Fleet orders of more \
         than 12 units ship on a pallet and are scheduled with a freight appointment, which adds \
         3 to 5 business days to the standard estimate.\n\n\
         Tracking is emailed when the label is created and again when the carrier makes the first \
         scan. A shipment with no carrier scan 48 hours after the label was created is treated as \
         lost in transit and is reshipped at no charge.",
    ),
    (
        "policies/international-shipping.md",
        "International shipping",
        "International shipping is available to 31 countries and takes 10 to 21 business days. \
         Customs duties, import taxes, and brokerage fees are the responsibility of the \
         recipient and are collected by the carrier at delivery.\n\n\
         Northwind ships internationally on Delivered At Place terms. We cannot pre-pay duties or \
         mark a shipment as a gift. Some countries restrict lithium battery imports; in those \
         destinations the unit ships without battery packs and the packs are sourced locally by \
         our distributor.",
    ),
    (
        "policies/warranty.md",
        "Limited warranty",
        "Every Atlas unit carries a 2-year limited warranty from the date of delivery. The \
         warranty covers manufacturing defects in the chassis, drive train, encoders, and \
         mainboard, and covers battery packs that fall below 70 percent of rated capacity within \
         the term.\n\n\
         The warranty does not cover water damage, damage from operating the unit outside its \
         rated IP54 environment, damage from third-party end effectors, or cosmetic wear. Water \
         damage is determined by the internal moisture indicator strip, and a tripped strip voids \
         coverage on the affected assembly.\n\n\
         Warranty service is repair-or-replace at Northwind's option. Advance replacement is \
         available on Fleet Pro subscriptions.",
    ),
    (
        "billing/subscription-tiers.md",
        "Subscription tiers",
        "Fleet Basic is 49 dollars per robot per month and includes telemetry dashboards, \
         firmware updates, and business-hours email support.\n\n\
         Fleet Pro is 129 dollars per robot per month and adds advance replacement, 24/7 phone \
         support, the fleet routing API, and 4 hours of monthly integration engineering. Annual \
         billing on either tier is discounted 2 months. Tier changes take effect at the next \
         billing cycle and are prorated to the day.",
    ),
    (
        "billing/invoices.md",
        "Enterprise invoicing",
        "Enterprise accounts may be invoiced on net-30 terms after a credit review. Invoices are \
         issued on the first business day of the month and cover the prior month's usage.\n\n\
         Purchase order numbers can be attached per invoice in the billing console. Past-due \
         invoices accrue 1.5 percent monthly interest and suspend advance replacement until \
         cleared.\n\n\
         Consolidated billing rolls every child account under one parent invoice; the parent \
         account owner sets it up in the billing console and it applies from the next invoice \
         issued, never retroactively to an invoice already sent.",
    ),
    (
        "product/atlas-r7-specs.md",
        "Atlas R7 specifications",
        "The Atlas R7 is the current-generation autonomous floor unit. Battery runtime is 6.5 \
         hours of continuous operation on a single pack and 13 hours with the dual-pack tray. \
         Maximum payload is 12 kilograms.\n\n\
         The R7 is rated IP54, runs the Northwind Sightline navigation stack, and carries a \
         64-channel lidar with a 25 meter range. Top speed is 1.8 meters per second. The R7 \
         mainboard is not compatible with R5 end effectors without the adapter collar.",
    ),
    (
        "product/atlas-r5-specs.md",
        "Atlas R5 specifications",
        "The Atlas R5 is the previous-generation autonomous floor unit, sold through 2023 and \
         still supported. Battery runtime is 4 hours of continuous operation. Maximum payload is \
         8 kilograms.\n\n\
         The R5 is rated IP52, runs the legacy Waypoint navigation stack, and carries a \
         16-channel lidar with a 12 meter range. Top speed is 1.1 meters per second. R5 units \
         cannot be upgraded to the Sightline stack.",
    ),
    (
        "product/charging-dock.md",
        "Charging dock",
        "The Northwind charging dock recharges a battery pack from empty to 80 percent in 43 \
         minutes and to full in 95 minutes. The dock draws 1400 watts at peak and requires a \
         dedicated 20 amp circuit.\n\n\
         Docks self-report contact wear over telemetry and should have their contact plate \
         replaced every 4000 dock cycles. A unit will not begin a charge cycle if the pack \
         temperature is above 45 degrees Celsius; it waits and reports a cooling state.",
    ),
    (
        "support/firmware-update.md",
        "Firmware updates",
        "Firmware updates are published monthly and install automatically during the maintenance \
         window configured in the fleet console. A unit downloads the image while docked and \
         applies it on the next dock cycle.\n\n\
         To roll back to a previous firmware version, open the unit in the fleet console, choose \
         Firmware, and select Roll back to previous version. The rollback keeps the two most \
         recent images on the unit, so only one version back is available. A rollback requires \
         the unit to be docked and above 40 percent charge, and it clears the navigation map \
         cache, which the unit rebuilds on its next run.",
    ),
    (
        "support/error-codes.md",
        "Error codes",
        "E-204 is a wheel encoder fault: the drive controller stopped receiving ticks from one of \
         the four wheel encoders. Clean the encoder disc and reseat the ribbon connector; a \
         persistent E-204 means the encoder assembly needs replacement and is covered by the \
         limited warranty.\n\n\
         E-311 is a thermal shutdown raised when the mainboard exceeds 85 degrees Celsius. \
         E-118 is a lidar occlusion warning, usually a dirty lens. E-402 means the unit lost its \
         navigation map and must be re-taught the floor.",
    ),
    (
        "security/data-retention.md",
        "Telemetry data retention",
        "Raw telemetry — pose, battery, and fault events — is retained for 90 days and then \
         deleted. Aggregated daily metrics are retained for 24 months so year-over-year \
         utilization reporting keeps working.\n\n\
         Camera frames are never uploaded off the unit unless an operator explicitly attaches \
         one to a support ticket, in which case the frame is retained with the ticket for 12 \
         months. Customers on Fleet Pro may request a shorter retention window in writing.",
    ),
    (
        "policies/exchanges.md",
        "Exchanges",
        "An exchange swaps a delivered unit for a different model within the same 17-day window \
         that governs returns, measured from the delivery date. Exchanges use the same prepaid \
         label and require the original packaging.\n\n\
         Exchanges to a higher model tier are charged the price difference at the time of the \
         swap; exchanges downward are credited the difference. Only one exchange is permitted per \
         serial number. An exchange is not a return and does not restart the warranty term, which \
         continues to run from the original delivery date.",
    ),
    (
        "policies/cancellations.md",
        "Order cancellation",
        "An order can be cancelled at no charge any time before it ships. Once the carrier scans \
         the shipment, the order can no longer be cancelled and must be handled as a return.\n\n\
         A cancelled order is refunded to the original payment method within 3 business days. \
         Cancelled fleet orders that had a freight appointment scheduled may be charged the \
         carrier cancellation fee, which is passed through at cost.\n\n\
         Subscriptions are separate from hardware orders: cancelling an order does not cancel a \
         Fleet Basic or Fleet Pro subscription attached to other units, and a subscription \
         cancellation takes effect at the end of the billing cycle already paid for.",
    ),
    (
        "support/battery-care.md",
        "Battery pack care",
        "Store spare battery packs at roughly 50 percent charge in a dry space between 5 and 25 \
         degrees Celsius. A pack stored full or empty for months loses capacity permanently.\n\n\
         Rated runtime degrades with cycle count; expect roughly 85 percent of original runtime \
         after 800 charge cycles. A pack that falls below 70 percent of rated capacity inside the \
         warranty term is replaced at no charge. Never charge a pack that has been dropped or \
         shows swelling.",
    ),
    (
        "product/end-effectors.md",
        "End effectors",
        "Northwind sells three first-party end effectors: the shelf tray, the tote gripper, and \
         the tow hitch. Each carries its own payload limit, which is lower than the chassis \
         maximum: 9 kilograms for the gripper and 12 kilograms for the tray.\n\n\
         Third-party effectors mount with the adapter collar but are not covered by the limited \
         warranty, and an effector that exceeds the chassis payload limit will trip the drive \
         controller. Effectors are not hot-swappable; power the unit down before changing one.",
    ),
    (
        "support/network-setup.md",
        "Network setup",
        "A unit provisions onto WiFi from the fleet console using a one-time pairing code. Both \
         2.4 and 5 GHz bands are supported; the unit prefers 5 GHz when the signal is above -65 \
         dBm.\n\n\
         The unit needs outbound access on 443 to reach telemetry and to download firmware \
         images. A unit that cannot reach the network still operates and buffers telemetry for up \
         to 7 days, but it will not receive a firmware update until connectivity is restored.\n\n\
         Captive-portal networks are not supported; the unit cannot complete a browser-based \
         sign-in. Use a pre-shared key or a certificate profile pushed from the console instead, \
         and keep the unit on the same VLAN as the dock it is registered to.",
    ),
    (
        "billing/taxes.md",
        "Sales tax and VAT",
        "Sales tax is calculated on the ship-to address and is collected at checkout in the 24 \
         states where Northwind has nexus. Tax collected is refunded along with the purchase \
         price when an order is returned.\n\n\
         International orders are billed exclusive of VAT; VAT and import duties are assessed by \
         the destination country and collected by the carrier. Tax exemption certificates are \
         uploaded in the billing console and apply from the next order onward.\n\n\
         Marketplace and reseller purchases are taxed by the reseller, not by Northwind, so a tax \
         question on a reseller invoice has to go back to the reseller. Northwind cannot re-issue \
         a reseller invoice or adjust the tax on one.",
    ),
    (
        "support/diagnostics.md",
        "Running diagnostics",
        "The diagnostics panel in the fleet console reads the unit fault log, live encoder ticks \
         per wheel, mainboard temperature, and lidar return rate. Run it before opening a support \
         ticket; the export attaches the last 500 fault entries.\n\n\
         A wheel showing zero ticks while the others count is a drive-side fault, not a \
         navigation problem. Temperatures above 80 degrees Celsius during a normal run indicate a \
         blocked intake. The panel does not clear faults; faults clear on the next successful \
         dock cycle.",
    ),
];

/// The frozen labeled query set.
///
/// Each query is phrased in the keyword style `knowledge_search`'s schema
/// instructs the model to use ("phrase it with the key terms you expect to
/// appear in the answer"), because that — not conversational English — is the
/// actual input distribution the retriever sees in production.
const QUERIES: &[LabeledQuery] = &[
    LabeledQuery {
        query: "return window delivery date days",
        relevant: &["policies/returns.md"],
    },
    LabeledQuery {
        query: "refund original payment method business days",
        relevant: &["billing/refunds.md"],
    },
    LabeledQuery {
        query: "freight appointment pallet fleet orders",
        relevant: &["policies/shipping.md"],
    },
    LabeledQuery {
        query: "lithium battery imports restricted destinations",
        relevant: &["policies/international-shipping.md"],
    },
    LabeledQuery {
        query: "warranty term manufacturing defects covered",
        relevant: &["policies/warranty.md"],
    },
    LabeledQuery {
        query: "water damage moisture indicator coverage",
        relevant: &["policies/warranty.md"],
    },
    LabeledQuery {
        query: "r7 top speed lidar channel meter range",
        relevant: &["product/atlas-r7-specs.md"],
    },
    LabeledQuery {
        query: "contact plate replaced dock cycles",
        relevant: &["product/charging-dock.md"],
    },
    LabeledQuery {
        query: "thermal shutdown mainboard exceeds degrees",
        relevant: &["support/error-codes.md"],
    },
    LabeledQuery {
        query: "firmware roll back previous version",
        relevant: &["support/firmware-update.md"],
    },
    LabeledQuery {
        query: "raw telemetry retention deleted",
        relevant: &["security/data-retention.md"],
    },
    LabeledQuery {
        query: "fleet pro dollars per robot per month",
        relevant: &["billing/subscription-tiers.md"],
    },
    LabeledQuery {
        query: "exchange swap different model tier",
        relevant: &["policies/exchanges.md"],
    },
    LabeledQuery {
        query: "cancel order before carrier scans shipment",
        relevant: &["policies/cancellations.md"],
    },
    LabeledQuery {
        query: "store spare packs percent charge capacity loss",
        relevant: &["support/battery-care.md"],
    },
    LabeledQuery {
        query: "third party effector adapter collar payload limit",
        relevant: &["product/end-effectors.md"],
    },
    LabeledQuery {
        query: "outbound 443 buffers telemetry firmware images",
        relevant: &["support/network-setup.md"],
    },
    LabeledQuery {
        query: "sales tax nexus ship-to address checkout",
        relevant: &["billing/taxes.md"],
    },
    LabeledQuery {
        query: "fault log live encoder ticks mainboard temperature panel",
        relevant: &["support/diagnostics.md"],
    },
    // Multi-document ground truth: answering this needs BOTH the return window
    // and the refund settlement time, so recall@k can be partial (0.5) rather
    // than only 0 or 1.
    LabeledQuery {
        query: "return window refund settlement timeline",
        relevant: &["policies/returns.md", "billing/refunds.md"],
    },
];

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    /// Every label must name a document that actually exists in the corpus. A
    /// typo'd label silently caps recall at less than 1.0 forever and would get
    /// "fixed" by lowering the threshold — this catches it as a hard failure.
    #[test]
    fn every_label_names_a_real_corpus_document() {
        let sources: HashSet<&str> = DOCS.iter().map(|(s, _, _)| *s).collect();
        for q in QUERIES {
            assert!(!q.relevant.is_empty(), "query {:?} has no labels", q.query);
            for label in q.relevant {
                assert!(
                    sources.contains(label),
                    "query {:?} labels unknown source {label:?}",
                    q.query
                );
            }
        }
    }

    /// Sources are the ground-truth identity; duplicates would merge two
    /// documents into one label and quietly inflate recall.
    #[test]
    fn corpus_sources_are_unique() {
        let mut seen = HashSet::new();
        for (source, _, _) in DOCS {
            assert!(seen.insert(*source), "duplicate corpus source {source:?}");
        }
    }

    /// The corpus must actually exercise the chunker: at least a few documents
    /// have to exceed the default 500-char cap, or a chunker regression cannot
    /// show up in the retrieval numbers at all.
    #[test]
    fn corpus_exercises_the_chunker() {
        let long = DOCS
            .iter()
            .filter(|(_, _, body)| body.len() > smooth_operator_ingestion::DEFAULT_MAX_CHARS)
            .count();
        assert!(
            long >= 8,
            "only {long} corpus docs exceed the chunk cap; the chunker is barely exercised"
        );
    }
}
