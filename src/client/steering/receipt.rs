//! UI projection of the client-owned queue, never an enqueue acknowledgement.
use crate::wire::SteeringDelivery;
use serde_json::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::client) enum Receipt {
    Waiting,
    Sending,
    Offline,
    Editing,
    Recalling,
    Cancelling,
    Cancelled,
    Review,
    Unconfirmed,
    Read,
}

impl Receipt {
    pub(in crate::client) fn from_entry(entry: &Value) -> Self {
        match entry["status"].as_str().unwrap_or_default() {
            "delivered" => {
                let delivered =
                    serde_json::from_value::<SteeringDelivery>(entry["delivery"].clone())
                        .is_ok_and(|delivery| {
                            !delivery.run_id.is_empty() && delivery.included_at_ms > 0
                        });
                if delivered {
                    Self::Read
                } else {
                    Self::Unconfirmed
                }
            }
            "cancelled" => Self::Cancelled,
            _ if entry["uncertain"] == true || entry["retarget_required"] == true => Self::Review,
            "queued" => Self::Waiting,
            "sending" => Self::Sending,
            "offline" => Self::Offline,
            "editing" => Self::Editing,
            "taking-back" => Self::Recalling,
            "cancelling" => Self::Cancelling,
            _ => Self::Review,
        }
    }

    pub(in crate::client) fn label(self) -> &'static str {
        match self {
            Self::Waiting => "Waiting",
            Self::Sending => "Confirming",
            Self::Offline => "Offline",
            Self::Editing => "Editing",
            Self::Recalling => "Recalling",
            Self::Cancelling => "Cancelling",
            Self::Cancelled => "Cancelled",
            Self::Review => "Review required",
            Self::Unconfirmed => "Delivery unconfirmed",
            Self::Read => "Read",
        }
    }

    pub(in crate::client) fn marker(self) -> char {
        if self == Self::Read { '●' } else { '○' }
    }

    pub(in crate::client) fn editable(self) -> bool {
        matches!(self, Self::Waiting | Self::Offline | Self::Editing)
    }

    pub(in crate::client) fn pending(self) -> bool {
        matches!(
            self,
            Self::Waiting
                | Self::Sending
                | Self::Offline
                | Self::Editing
                | Self::Recalling
                | Self::Cancelling
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn receipt_requires_a_real_delivery_record_and_terminal_state() {
        let delivery = json!({"run_id":"run_one","agent_id":null,"included_at_ms":1000});
        for status in [
            "queued",
            "sending",
            "offline",
            "editing",
            "taking-back",
            "cancelling",
            "cancelled",
            "uncertain",
        ] {
            let entry = json!({"status":status,"delivery":delivery});
            assert_ne!(Receipt::from_entry(&entry), Receipt::Read);
        }
        for delivery in [
            Value::Null,
            json!({}),
            json!({"run_id":"","included_at_ms":1000}),
            json!({"run_id":"run_one","included_at_ms":0}),
            json!({"run_id":"run_one"}),
        ] {
            assert_eq!(
                Receipt::from_entry(&json!({"status":"delivered","delivery":delivery})),
                Receipt::Unconfirmed
            );
        }
        assert_eq!(
            Receipt::from_entry(&json!({"status":"delivered","delivery":delivery})),
            Receipt::Read
        );
        assert!(!Receipt::Read.editable());
        assert!(!Receipt::Unconfirmed.editable());
        assert!(!Receipt::Sending.editable());
    }
}
