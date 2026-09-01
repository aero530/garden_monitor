//! Changing a password, against a real database.
//!
//! This exists because a self-hosted server has no "forgot password" email to fall back
//! on and no support desk behind it. The account created on first registration owns
//! everything, and until now nothing in the codebase could ever change its password —
//! so a database carried from one machine to another carried whatever credential it was
//! set up with, permanently.

use garden_auth::{EmailAddress, WeakPassword};
use garden_store::Store;
use garden_store::accounts::ChangePassword;

fn t0() -> jiff::Timestamp {
    jiff::Timestamp::from_second(1_700_000_000).unwrap()
}

fn email(s: &str) -> EmailAddress {
    EmailAddress::parse(s).unwrap()
}

const OLD: &str = "the original password";
const NEW: &str = "a different long password";

/// A registered user, signed in, with their session token.
async fn signed_in() -> (Store, garden_auth::UserId, garden_auth::SecretToken) {
    let store = Store::in_memory().await.unwrap();
    let user = store
        .create_user(email("phil@example.com"), "Phil", OLD, t0())
        .await
        .unwrap();
    let (_, token) = store
        .authenticate(&email("phil@example.com"), OLD, t0(), None)
        .await
        .unwrap()
        .expect("the original password should work");
    (store, user.id, token)
}

#[tokio::test]
async fn a_changed_password_replaces_the_old_one() {
    let (store, user, token) = signed_in().await;

    let outcome = store.change_password(user, OLD, NEW, &token).await.unwrap();
    assert!(matches!(outcome, ChangePassword::Changed { .. }));

    assert!(
        store
            .authenticate(&email("phil@example.com"), NEW, t0(), None)
            .await
            .unwrap()
            .is_some(),
        "the new password should work"
    );
    assert!(
        store
            .authenticate(&email("phil@example.com"), OLD, t0(), None)
            .await
            .unwrap()
            .is_none(),
        "the old password must stop working"
    );
}

#[tokio::test]
async fn the_current_password_is_required() {
    // The difference between someone having borrowed your laptop for five minutes and
    // someone owning your garden for ever.
    let (store, user, token) = signed_in().await;

    let outcome = store
        .change_password(user, "not the password", NEW, &token)
        .await
        .unwrap();
    assert_eq!(outcome, ChangePassword::WrongPassword);

    assert!(
        store
            .authenticate(&email("phil@example.com"), OLD, t0(), None)
            .await
            .unwrap()
            .is_some(),
        "the original password should be untouched"
    );
}

#[tokio::test]
async fn a_weak_new_password_is_refused() {
    let (store, user, token) = signed_in().await;
    let outcome = store.change_password(user, OLD, "short", &token).await.unwrap();
    assert_eq!(outcome, ChangePassword::TooWeak(WeakPassword::TooShort));
}

#[tokio::test]
async fn the_policy_is_not_an_oracle_for_the_current_password() {
    // Both checks are needed, and the order matters. If the weak-password complaint
    // came first, a stranger could tell a correct guess of the current password from an
    // incorrect one by which message came back.
    let (store, user, token) = signed_in().await;
    let outcome = store
        .change_password(user, "the wrong current password", "short", &token)
        .await
        .unwrap();
    assert_eq!(
        outcome,
        ChangePassword::WrongPassword,
        "a wrong current password must be reported before the new one is judged"
    );
}

#[tokio::test]
async fn other_sessions_are_closed_but_your_own_survives() {
    // Changing a password is what you do when you think someone else is signed in.
    // Leaving their session alive would make it ceremonial — and logging yourself out
    // as well is the kind of small rudeness that stops people doing it at all.
    let (store, user, token) = signed_in().await;

    let elsewhere = store
        .authenticate(&email("phil@example.com"), OLD, t0(), Some("a phone".into()))
        .await
        .unwrap()
        .expect("a second sign-in")
        .1;
    assert_eq!(store.sessions_of(user).await.unwrap().len(), 2);

    let outcome = store.change_password(user, OLD, NEW, &token).await.unwrap();
    assert_eq!(
        outcome,
        ChangePassword::Changed {
            other_sessions_closed: 1
        }
    );

    assert!(
        store.actor_for_token(&token, t0()).await.unwrap().is_some(),
        "the session that made the change should survive"
    );
    assert!(
        store.actor_for_token(&elsewhere, t0()).await.unwrap().is_none(),
        "the other session should be gone"
    );
}

#[tokio::test]
async fn one_account_cannot_change_anothers_password() {
    // The user id is taken from the caller's own session in the handler, but the store
    // is the layer that has to be right if that ever stops being true.
    let (store, phil, phils_token) = signed_in().await;
    let sam = store
        .create_user(email("sam@example.com"), "Sam", "sams long password", t0())
        .await
        .unwrap();

    // Phil's session, Sam's id, and Phil's password as the "current" one.
    let outcome = store
        .change_password(sam.id, OLD, NEW, &phils_token)
        .await
        .unwrap();
    assert_eq!(outcome, ChangePassword::WrongPassword);

    assert!(
        store
            .authenticate(&email("sam@example.com"), "sams long password", t0(), None)
            .await
            .unwrap()
            .is_some(),
        "Sam's password should be untouched"
    );
    let _ = phil;
}
