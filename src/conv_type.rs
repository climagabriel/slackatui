//! Which of Slack's conversation types a conversation is, and the line the
//! conversations pane draws under each type under the `Type` sort.
//!
//! The seven types are the ones Slack itself distinguishes, in the order the
//! pane lists them: public channels, private channels, Slack Connect channels,
//! direct messages, group direct messages, direct messages with an app, and
//! archived conversations of any kind. Every conversation lands in exactly one
//! of them, because the rule is a priority and not a set of tests that could
//! both answer yes: archived first, then a direct message with an app, then
//! Slack Connect, then the conversation's own kind. So an archived Slack
//! Connect channel is archived. Slack Connect here is a channel shared with
//! another organization: the shared flags move a public or private channel and
//! nothing else, so a direct or group message carrying one stays the direct or
//! group message it is.
//!
//! The pane draws a line under the last conversation of each type, so a type
//! with no conversations draws nothing, and only under `Type`: that is the one
//! sort whose order is the order these types are in.

use crate::archive::Kind;

/// The types in the order the pane lists them; `Ord` is that order, which is
/// what the sort compares on.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum ConvType {
    PublicChannel,
    PrivateChannel,
    SlackConnect,
    DirectMessage,
    GroupDirectMessage,
    AppDirectMessage,
    Archived,
}

impl ConvType {
    pub const fn label(self) -> &'static str {
        match self {
            ConvType::PublicChannel => "public channels",
            ConvType::PrivateChannel => "private channels",
            ConvType::SlackConnect => "Slack Connect channels",
            ConvType::DirectMessage => "direct messages",
            ConvType::GroupDirectMessage => "group direct messages",
            ConvType::AppDirectMessage => "direct messages with apps",
            ConvType::Archived => "archived",
        }
    }

    /// The type of a conversation of `kind`, given what the channel object
    /// said about it: `archived` is `is_archived`, `shared` is `is_shared` or
    /// `is_ext_shared` — the flags Slack sets on a conversation shared with
    /// another organization, which are authoritative — and `app` says the
    /// conversation is a direct message whose counterpart is a bot user, which
    /// is how an app appears in a direct message.
    ///
    /// A Slack Connect conversation is a *channel* shared with another
    /// organization, so the shared flags are read on a public or private
    /// channel only: an im or mpim carrying one is still the direct or group
    /// message it is.
    pub fn of(kind: Kind, archived: bool, shared: bool, app: bool) -> Self {
        if archived {
            return ConvType::Archived;
        }
        if app {
            return ConvType::AppDirectMessage;
        }
        if shared && matches!(kind, Kind::Channel | Kind::Private) {
            return ConvType::SlackConnect;
        }
        match kind {
            Kind::Channel => ConvType::PublicChannel,
            Kind::Private => ConvType::PrivateChannel,
            Kind::Mpim => ConvType::GroupDirectMessage,
            Kind::Im => ConvType::DirectMessage,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ConvType;
    use crate::archive::Kind;

    /// A plain conversation of each kind is the type of its kind.
    #[test]
    fn each_kind_is_its_own_type() {
        for (kind, want) in [
            (Kind::Channel, ConvType::PublicChannel),
            (Kind::Private, ConvType::PrivateChannel),
            (Kind::Mpim, ConvType::GroupDirectMessage),
            (Kind::Im, ConvType::DirectMessage),
        ] {
            assert_eq!(ConvType::of(kind, false, false, false), want, "{kind:?}");
        }
    }

    /// The shared flags move a public and a private channel to Slack Connect,
    /// and only those two: Slack Connect is a shared channel, so a direct or
    /// group message carrying a shared flag stays where its kind puts it.
    #[test]
    fn the_shared_flags_move_channels_only() {
        for kind in [Kind::Channel, Kind::Private] {
            assert_eq!(
                ConvType::of(kind, false, true, false),
                ConvType::SlackConnect,
                "{kind:?}"
            );
        }
        assert_eq!(
            ConvType::of(Kind::Im, false, true, false),
            ConvType::DirectMessage
        );
        assert_eq!(
            ConvType::of(Kind::Mpim, false, true, false),
            ConvType::GroupDirectMessage
        );
        assert_eq!(
            ConvType::of(Kind::Im, false, true, true),
            ConvType::AppDirectMessage
        );
    }

    /// The priority, tested where two rules could both answer: archived beats
    /// everything, an app direct message beats the shared flags, and the
    /// shared flags beat the kind.
    #[test]
    fn the_priority_decides_where_two_rules_could_both_answer() {
        // An archived Slack Connect channel, and an archived direct message
        // with an app, are archived.
        assert_eq!(ConvType::of(Kind::Channel, true, true, false), ConvType::Archived);
        assert_eq!(ConvType::of(Kind::Im, true, false, true), ConvType::Archived);
        assert_eq!(ConvType::of(Kind::Im, true, true, true), ConvType::Archived);
        // A shared direct message with an app is the app direct message, and
        // a shared channel with neither flag above it is Slack Connect.
        assert_eq!(
            ConvType::of(Kind::Im, false, true, true),
            ConvType::AppDirectMessage
        );
        assert_eq!(
            ConvType::of(Kind::Channel, false, true, false),
            ConvType::SlackConnect
        );
    }

    /// The declaration order is the order the pane lists them in, which is
    /// what the sort compares on: every type is below the one before it.
    #[test]
    fn the_types_are_ordered_the_way_the_pane_lists_them() {
        let order = [
            ConvType::PublicChannel,
            ConvType::PrivateChannel,
            ConvType::SlackConnect,
            ConvType::DirectMessage,
            ConvType::GroupDirectMessage,
            ConvType::AppDirectMessage,
            ConvType::Archived,
        ];
        for pair in order.windows(2) {
            assert!(pair[0] < pair[1], "{:?} before {:?}", pair[0], pair[1]);
        }
        // Every label is distinct, so a drawn line names one type only.
        let mut labels: Vec<&str> = order.iter().map(|t| t.label()).collect();
        labels.sort_unstable();
        labels.dedup();
        assert_eq!(labels.len(), order.len());
    }
}
