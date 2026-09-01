//! Administrative password reset, against a real database.
//!
//! The escape hatch for a self-hosted server: no reset email, no support desk, and a
//! registration form that closes as soon as the first account exists. Without this, a
//! forgotten password locks you out of your own history permanently.

use garden_auth::EmailAddress;
use garden_store::Store;

fn t0() -> jiff::Timestamp {
    jiff::Timestamp::from_second(1_700_000_000).unwrap()
}

fn email(s: &str) -> EmailAddress {
    EmailAddress::parse(s).unwrap()
}

#[tokio::test]
async fn a_forgotten_password_can_be_replaced_without_knowing_it() {
    let store = Store::in_memory().await.unwrap();
    let user = store
        .create_user(email("phil@example.com"), "Phil", "the forgotten one", t0())
        .await
        .unwrap();

    store.reset_password(user.id, "a brand new password").await.unwrap();

    assert!(
        store
            .authenticate(&email("phil@example.com"), "a brand new password", t0(), None)
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        store
            .authenticate(&email("phil@example.com"), "the forgotten one", t0(), None)
            .await
            .unwrap()
            .is_none(),
        "the old password must stop working"
    );
}

#[tokio::test]
async fn a_reset_signs_everyone_out() {
    // Including whoever is holding the session you could not use. A reset is what you
    // do when you have lost control of an account, so leaving sessions alive would
    // defeat it.
    let store = Store::in_memory().await.unwrap();
    let user = store
        .create_user(email("phil@example.com"), "Phil", "the old password", t0())
        .await
        .unwrap();
    let token = store
        .authenticate(&email("phil@example.com"), "the old password", t0(), None)
        .await
        .unwrap()
        .unwrap()
        .1;

    store.reset_password(user.id, "a brand new password").await.unwrap();

    assert!(store.actor_for_token(&token, t0()).await.unwrap().is_none());
    assert!(store.sessions_of(user.id).await.unwrap().is_empty());
}

#[tokio::test]
async fn the_policy_still_applies() {
    // Being the administrator is authorisation to set a password, not permission to set
    // a bad one.
    let store = Store::in_memory().await.unwrap();
    let user = store
        .create_user(email("phil@example.com"), "Phil", "the old password", t0())
        .await
        .unwrap();

    assert!(store.reset_password(user.id, "short").await.is_err());
    assert!(
        store
            .authenticate(&email("phil@example.com"), "the old password", t0(), None)
            .await
            .unwrap()
            .is_some(),
        "a refused reset must leave the old password working"
    );
}

#[tokio::test]
async fn resetting_an_account_that_does_not_exist_says_so() {
    let store = Store::in_memory().await.unwrap();
    let nobody = garden_auth::UserId::new();
    assert!(store.reset_password(nobody, "a long enough password").await.is_err());
}

#[tokio::test]
async fn an_account_can_be_promoted_to_administrator() {
    // A database can outlive the person who made it, or arrive by migration with the
    // admin flag on somebody who is no longer around.
    let store = Store::in_memory().await.unwrap();
    let first = store
        .create_user(email("phil@example.com"), "Phil", "a long enough password", t0())
        .await
        .unwrap();
    let second = store
        .create_user(email("sam@example.com"), "Sam", "a long enough password", t0())
        .await
        .unwrap();
    assert!(first.is_admin, "the first account is administrator");
    assert!(!second.is_admin);

    store.make_admin(second.id).await.unwrap();
    assert!(store.find_user(second.id).await.unwrap().unwrap().is_admin);
}
