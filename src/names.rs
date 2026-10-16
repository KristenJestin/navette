//! What a sentence calls the two machines: the user's own labels (`local_label` for the desk,
//! `remote_label` for the remote machine) when they are set, else `desk` and `remote`.

/// The two machines' names, as the user reads them in a notification, a corner or an answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Names {
    pub desk: String,
    pub remote: String,
}

impl Default for Names {
    fn default() -> Self {
        Self::new(None, None)
    }
}

impl Names {
    /// An empty label is no label: the file's `local_label =` says nothing.
    pub fn new(desk: Option<&str>, remote: Option<&str>) -> Self {
        let name = |label: Option<&str>, fallback: &str| {
            label
                .filter(|label| !label.is_empty())
                .unwrap_or(fallback)
                .to_string()
        };
        Self {
            desk: name(desk, "desk"),
            remote: name(remote, "remote"),
        }
    }

    /// The desk as a sentence puts it after a preposition: `the desk`, or the label as it is.
    pub fn the_desk(&self) -> String {
        match self.desk.as_str() {
            "desk" => "the desk".to_string(),
            label => label.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn with_no_label_the_machines_are_the_desk_and_the_remote() {
        let names = Names::default();
        assert_eq!(
            (names.desk.as_str(), names.remote.as_str()),
            ("desk", "remote")
        );
    }

    #[test]
    fn a_label_that_is_set_names_its_machine_and_only_its_machine() {
        let names = Names::new(Some("NEAR"), None);
        assert_eq!(
            (names.desk.as_str(), names.remote.as_str()),
            ("NEAR", "remote")
        );
        let names = Names::new(None, Some("FAR"));
        assert_eq!(
            (names.desk.as_str(), names.remote.as_str()),
            ("desk", "FAR")
        );
    }

    #[test]
    fn after_a_preposition_the_desk_is_the_desk_and_a_label_stays_as_it_is() {
        assert_eq!(Names::default().the_desk(), "the desk");
        assert_eq!(Names::new(Some("NEAR"), None).the_desk(), "NEAR");
    }

    #[test]
    fn an_empty_label_is_no_label() {
        assert_eq!(Names::new(Some(""), Some("")), Names::default());
    }
}
