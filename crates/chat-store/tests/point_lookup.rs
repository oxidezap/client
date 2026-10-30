mod common;
use common::*;

#[tokio::test]
async fn contact_lookup_keeps_account_labels_optional_fields_and_missing_rows() {
    let (db, store) = test_store().await;
    db.database().provision_device(2).await.unwrap();
    db.shared().run(|conn| {
        diesel::sql_query("INSERT INTO contacts(device_id,jid,push_name,full_name,first_name,business_name) VALUES(1,'559900000001@s.whatsapp.net','push','full','first','business'),(2,'559900000001@s.whatsapp.net','other','other','other','other'),(1,'559900000002@s.whatsapp.net',NULL,NULL,NULL,NULL)")
            .execute(conn).map_err(db_err)?;
        diesel::sql_query("INSERT INTO contact_labels(device_id,jid,alias,tags) VALUES(1,'559900000001@s.whatsapp.net','alias','[\"tag\"]'),(2,'559900000001@s.whatsapp.net','other','[]')")
            .execute(conn).map_err(db_err)?;
        Ok(())
    }).await.unwrap();
    for _ in 0..3 {
        let contact = store
            .contact(&jid("559900000001:7@s.whatsapp.net"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(contact.jid, jid(PEER));
        assert_eq!(contact.push_name.as_deref(), Some("push"));
        assert_eq!(contact.full_name.as_deref(), Some("full"));
        assert_eq!(contact.first_name.as_deref(), Some("first"));
        assert_eq!(contact.business_name.as_deref(), Some("business"));
        assert_eq!(contact.alias.as_deref(), Some("alias"));
        assert_eq!(contact.tags, vec!["tag"]);
        let blank = store
            .contact(&jid("559900000002@s.whatsapp.net"))
            .await
            .unwrap()
            .unwrap();
        assert!(
            blank.full_name.is_none()
                && blank.push_name.is_none()
                && blank.first_name.is_none()
                && blank.business_name.is_none()
                && blank.alias.is_none()
        );
        assert!(blank.tags.is_empty());
        assert!(
            store
                .contact(&jid("559900000003@s.whatsapp.net"))
                .await
                .unwrap()
                .is_none()
        );
    }
}
