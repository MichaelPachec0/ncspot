use zbus::interface;

pub struct MprisRoot {}

#[interface(name = "org.mpris.MediaPlayer2")]
impl MprisRoot {
    #[zbus(property)]
    fn can_quit(&self) -> bool {
        false
    }

    #[zbus(property)]
    fn can_raise(&self) -> bool {
        false
    }

    /// Exported as `HasTrackList`, the name in the MPRIS specification.
    #[zbus(property)]
    fn has_track_list(&self) -> bool {
        true
    }

    #[zbus(property)]
    fn identity(&self) -> &str {
        "ncspot"
    }

    #[zbus(property)]
    fn supported_uri_schemes(&self) -> Vec<String> {
        vec!["spotify".to_string()]
    }

    #[zbus(property)]
    fn supported_mime_types(&self) -> Vec<String> {
        Vec::new()
    }

    fn raise(&self) {}

    fn quit(&self) {}
}

#[cfg(test)]
mod tests {
    use zbus::object_server::Interface;

    use super::MprisRoot;

    #[test]
    fn has_track_list_is_exported_under_its_mpris_name() {
        let mut xml = String::new();
        MprisRoot {}.introspect_to_writer(&mut xml, 0);
        assert!(
            xml.contains(r#"<property name="HasTrackList" type="b""#),
            "{xml}"
        );
        assert!(!xml.contains("HasTracklist"), "{xml}");
    }
}
