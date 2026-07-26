//! Multi-tenancy, against a real database.
//!
//! The unit tests in `garden-auth` prove the policy is right. These prove the queries
//! agree with it — that no join, filter, or cascade quietly hands one account access
//! to another's garden. That is the failure mode that matters here: it is silent,
//! it is a privacy breach, and it is not visible from reading the policy alone.

use garden_auth::{Actor, EmailAddress, Invitation, Membership, Permission, Role};
use garden_core::DeviceModel;
use garden_store::Store;

fn t0() -> jiff::Timestamp {
    jiff::Timestamp::from_second(1_700_000_000).unwrap()
}

fn email(s: &str) -> EmailAddress {
    EmailAddress::parse(s).unwrap()
}

async fn fixture() -> Store {
    Store::in_memory().await.unwrap()
}

async fn actor_for(store: &Store, user: garden_auth::UserId) -> Actor {
    let u = store.find_user(user).await.unwrap().unwrap();
    let memberships = store.memberships_of_user(user).await.unwrap();
    Actor::new(u, memberships)
}

#[tokio::test]
async fn the_first_account_is_the_administrator_and_later_ones_are_not() {
    let store = fixture().await;
    let first = store
        .create_user(email("phil@example.com"), "Phil", "a long enough password", t0())
        .await
        .unwrap();
    let second = store
        .create_user(email("sam@example.com"), "Sam", "a long enough password", t0())
        .await
        .unwrap();

    assert!(first.is_admin);
    assert!(!second.is_admin);
}

#[tokio::test]
async fn an_address_can_only_register_once() {
    let store = fixture().await;
    store
        .create_user(email("phil@example.com"), "Phil", "a long enough password", t0())
        .await
        .unwrap();
    let again = store
        .create_user(email("PHIL@example.com"), "Impostor", "another long password", t0())
        .await;
    assert!(matches!(again, Err(garden_store::StoreError::EmailTaken)));
}

#[tokio::test]
async fn one_account_holds_several_gardens() {
    let store = fixture().await;
    let phil = store
        .create_user(email("phil@example.com"), "Phil", "a long enough password", t0())
        .await
        .unwrap();

    for name in ["Kitchen", "Office", "Basement"] {
        store
            .create_garden(name, DeviceModel::Studio2, "UTC", phil.id, t0())
            .await
            .unwrap();
    }

    let listings = store.gardens_for_user(phil.id).await.unwrap();
    assert_eq!(listings.len(), 3);
    assert!(listings.iter().all(|l| l.role == Role::Owner));
    assert!(listings.iter().all(|l| !l.is_shared()));
}

#[tokio::test]
async fn gardens_are_invisible_to_everyone_who_is_not_a_member() {
    let store = fixture().await;
    let phil = store
        .create_user(email("phil@example.com"), "Phil", "a long enough password", t0())
        .await
        .unwrap();
    let mallory = store
        .create_user(email("mallory@example.com"), "Mallory", "a long enough password", t0())
        .await
        .unwrap();

    let garden = store
        .create_garden("Kitchen", DeviceModel::Studio2, "UTC", phil.id, t0())
        .await
        .unwrap();

    assert!(store.gardens_for_user(mallory.id).await.unwrap().is_empty());

    // Even holding the id — which appears in URLs — grants nothing.
    let actor = actor_for(&store, mallory.id).await;
    assert_eq!(actor.role_in(garden.id), None);
    let denied = actor.require(garden.id, Permission::ViewGarden).unwrap_err();
    assert!(
        denied.conceals_existence(),
        "must 404, not 403 — otherwise the id is an existence oracle"
    );
}

#[tokio::test]
async fn a_server_administrator_still_cannot_see_someone_elses_garden() {
    let store = fixture().await;
    // Phil registers first, so Phil is the admin.
    let phil = store
        .create_user(email("phil@example.com"), "Phil", "a long enough password", t0())
        .await
        .unwrap();
    let sam = store
        .create_user(email("sam@example.com"), "Sam", "a long enough password", t0())
        .await
        .unwrap();
    assert!(phil.is_admin);

    let sams_garden = store
        .create_garden("Sam's tower", DeviceModel::Studio2, "UTC", sam.id, t0())
        .await
        .unwrap();

    let admin = actor_for(&store, phil.id).await;
    assert!(admin.require_admin().is_ok());
    assert!(admin.require(sams_garden.id, Permission::ViewGarden).is_err());
    assert!(store.gardens_for_user(phil.id).await.unwrap().is_empty());
}

#[tokio::test]
async fn sharing_a_garden_end_to_end() {
    let store = fixture().await;
    let phil = store
        .create_user(email("phil@example.com"), "Phil", "a long enough password", t0())
        .await
        .unwrap();
    let sam = store
        .create_user(email("sam@example.com"), "Sam", "a long enough password", t0())
        .await
        .unwrap();

    let garden = store
        .create_garden("Kitchen", DeviceModel::Studio2, "UTC", phil.id, t0())
        .await
        .unwrap();

    // Phil invites Sam as a caretaker.
    let (mut invitation, token) = Invitation::issue(
        garden.id,
        sam.email.clone(),
        Role::Caretaker,
        phil.id,
        Role::Owner,
        t0(),
    )
    .unwrap();
    store.create_invitation(&invitation).await.unwrap();

    // The link resolves only by its digest, never by the stored secret.
    let found = store.find_invitation_by_token(&token).await.unwrap().unwrap();
    assert_eq!(found.id, invitation.id);

    let role = invitation.accept(&sam, t0()).unwrap();
    store.save_invitation(&invitation).await.unwrap();
    store
        .grant_membership(&Membership::granted(garden.id, sam.id, role, phil.id, t0()))
        .await
        .unwrap();

    // Sam now sees it, as a caretaker, and knows it is not his.
    let listings = store.gardens_for_user(sam.id).await.unwrap();
    assert_eq!(listings.len(), 1);
    assert_eq!(listings[0].role, Role::Caretaker);
    assert!(listings[0].is_shared());
    assert!(listings[0].is_someone_elses());

    let sam_actor = actor_for(&store, sam.id).await;
    assert!(sam_actor.can(garden.id, Permission::CompleteTask));
    assert!(!sam_actor.can(garden.id, Permission::ManageMembers));
    assert!(!sam_actor.can(garden.id, Permission::DeleteGarden));

    // And Phil's view now shows two people.
    let members = store.members_of(garden.id).await.unwrap();
    assert_eq!(members.len(), 2);
    assert_eq!(members[0].role, Role::Owner, "owner sorts first");
}

#[tokio::test]
async fn revoking_a_membership_removes_access_immediately() {
    let store = fixture().await;
    let phil = store
        .create_user(email("phil@example.com"), "Phil", "a long enough password", t0())
        .await
        .unwrap();
    let sam = store
        .create_user(email("sam@example.com"), "Sam", "a long enough password", t0())
        .await
        .unwrap();
    let garden = store
        .create_garden("Kitchen", DeviceModel::Studio2, "UTC", phil.id, t0())
        .await
        .unwrap();
    store
        .grant_membership(&Membership::granted(
            garden.id,
            sam.id,
            Role::Caretaker,
            phil.id,
            t0(),
        ))
        .await
        .unwrap();
    assert_eq!(store.gardens_for_user(sam.id).await.unwrap().len(), 1);

    store.revoke_membership(garden.id, sam.id).await.unwrap();

    assert!(store.gardens_for_user(sam.id).await.unwrap().is_empty());
    let actor = actor_for(&store, sam.id).await;
    assert!(actor.require(garden.id, Permission::ViewGarden).is_err());
}

#[tokio::test]
async fn deleting_a_garden_takes_every_membership_with_it() {
    let store = fixture().await;
    let phil = store
        .create_user(email("phil@example.com"), "Phil", "a long enough password", t0())
        .await
        .unwrap();
    let sam = store
        .create_user(email("sam@example.com"), "Sam", "a long enough password", t0())
        .await
        .unwrap();
    let garden = store
        .create_garden("Kitchen", DeviceModel::Studio2, "UTC", phil.id, t0())
        .await
        .unwrap();
    store
        .grant_membership(&Membership::granted(
            garden.id,
            sam.id,
            Role::Viewer,
            phil.id,
            t0(),
        ))
        .await
        .unwrap();

    store.delete_garden(garden.id).await.unwrap();

    // No orphaned grants pointing at a garden that no longer exists.
    assert!(store.memberships_of_user(sam.id).await.unwrap().is_empty());
    assert!(store.memberships_of_user(phil.id).await.unwrap().is_empty());
    assert!(store.find_garden(garden.id).await.unwrap().is_none());
}

#[tokio::test]
async fn transferring_ownership_swaps_the_two_roles() {
    let store = fixture().await;
    let phil = store
        .create_user(email("phil@example.com"), "Phil", "a long enough password", t0())
        .await
        .unwrap();
    let sam = store
        .create_user(email("sam@example.com"), "Sam", "a long enough password", t0())
        .await
        .unwrap();
    let garden = store
        .create_garden("Kitchen", DeviceModel::Studio2, "UTC", phil.id, t0())
        .await
        .unwrap();
    store
        .grant_membership(&Membership::granted(
            garden.id,
            sam.id,
            Role::Steward,
            phil.id,
            t0(),
        ))
        .await
        .unwrap();

    store
        .transfer_ownership(garden.id, phil.id, sam.id, t0())
        .await
        .unwrap();

    assert_eq!(store.role_of(garden.id, sam.id).await.unwrap(), Some(Role::Owner));
    // The former owner keeps working access but loses the ability to delete.
    assert_eq!(
        store.role_of(garden.id, phil.id).await.unwrap(),
        Some(Role::Steward)
    );
}

#[tokio::test]
async fn a_garden_cannot_be_handed_to_a_stranger() {
    let store = fixture().await;
    let phil = store
        .create_user(email("phil@example.com"), "Phil", "a long enough password", t0())
        .await
        .unwrap();
    let stranger = store
        .create_user(email("nobody@example.com"), "Nobody", "a long enough password", t0())
        .await
        .unwrap();
    let garden = store
        .create_garden("Kitchen", DeviceModel::Studio2, "UTC", phil.id, t0())
        .await
        .unwrap();

    let result = store
        .transfer_ownership(garden.id, phil.id, stranger.id, t0())
        .await;
    assert!(matches!(result, Err(garden_store::StoreError::NotFound)));
    assert_eq!(
        store.role_of(garden.id, phil.id).await.unwrap(),
        Some(Role::Owner),
        "a failed transfer must not strand the garden"
    );
}

#[tokio::test]
async fn a_session_resolves_to_its_owner_and_no_one_else() {
    let store = fixture().await;
    let phil = store
        .create_user(email("phil@example.com"), "Phil", "a long enough password", t0())
        .await
        .unwrap();
    store
        .create_user(email("sam@example.com"), "Sam", "a long enough password", t0())
        .await
        .unwrap();

    let (_, token) = store
        .authenticate(&email("phil@example.com"), "a long enough password", t0(), None)
        .await
        .unwrap()
        .unwrap();

    let actor = store.actor_for_token(&token, t0()).await.unwrap().unwrap();
    assert_eq!(actor.id(), phil.id);
}

#[tokio::test]
async fn a_wrong_password_yields_no_session() {
    let store = fixture().await;
    store
        .create_user(email("phil@example.com"), "Phil", "a long enough password", t0())
        .await
        .unwrap();

    let outcome = store
        .authenticate(&email("phil@example.com"), "wrong", t0(), None)
        .await
        .unwrap();
    assert!(outcome.is_none());
}

#[tokio::test]
async fn signing_in_as_a_nonexistent_account_fails_without_erroring() {
    // The decoy hash path — it must behave like an ordinary failed login.
    let store = fixture().await;
    let outcome = store
        .authenticate(&email("ghost@example.com"), "whatever", t0(), None)
        .await
        .unwrap();
    assert!(outcome.is_none());
}

#[tokio::test]
async fn an_expired_session_stops_working() {
    let store = fixture().await;
    let phil = store
        .create_user(email("phil@example.com"), "Phil", "a long enough password", t0())
        .await
        .unwrap();
    let token = store.open_session(phil.id, t0(), None).await.unwrap();

    assert!(store.actor_for_token(&token, t0()).await.unwrap().is_some());
    let much_later = garden_core::time::add_days(t0(), 60.0);
    assert!(store.actor_for_token(&token, much_later).await.unwrap().is_none());
}

#[tokio::test]
async fn signing_out_everywhere_kills_every_session() {
    let store = fixture().await;
    let phil = store
        .create_user(email("phil@example.com"), "Phil", "a long enough password", t0())
        .await
        .unwrap();
    let laptop = store.open_session(phil.id, t0(), None).await.unwrap();
    let phone = store.open_session(phil.id, t0(), None).await.unwrap();

    assert_eq!(store.close_all_sessions(phil.id).await.unwrap(), 2);
    assert!(store.actor_for_token(&laptop, t0()).await.unwrap().is_none());
    assert!(store.actor_for_token(&phone, t0()).await.unwrap().is_none());
}

#[tokio::test]
async fn a_forged_cookie_does_not_authenticate() {
    let store = fixture().await;
    store
        .create_user(email("phil@example.com"), "Phil", "a long enough password", t0())
        .await
        .unwrap();

    let forged = garden_auth::SecretToken::generate();
    assert!(store.actor_for_token(&forged, t0()).await.unwrap().is_none());
}

#[tokio::test]
async fn a_disabled_account_cannot_sign_in_or_use_an_existing_session() {
    let store = fixture().await;
    let phil = store
        .create_user(email("phil@example.com"), "Phil", "a long enough password", t0())
        .await
        .unwrap();
    let token = store.open_session(phil.id, t0(), None).await.unwrap();
    assert!(store.actor_for_token(&token, t0()).await.unwrap().is_some());

    sqlx::query("UPDATE users SET disabled_at = ?1 WHERE id = ?2")
        .bind(t0().to_string())
        .bind(phil.id.to_string())
        .execute(&store.db)
        .await
        .unwrap();

    // Live cookies must stop working, not merely new logins.
    assert!(store.actor_for_token(&token, t0()).await.unwrap().is_none());
    assert!(
        store
            .authenticate(&email("phil@example.com"), "a long enough password", t0(), None)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn events_stay_inside_their_own_garden() {
    let store = fixture().await;
    let phil = store
        .create_user(email("phil@example.com"), "Phil", "a long enough password", t0())
        .await
        .unwrap();
    let kitchen = store
        .create_garden("Kitchen", DeviceModel::Studio2, "UTC", phil.id, t0())
        .await
        .unwrap();
    let office = store
        .create_garden("Office", DeviceModel::Studio2, "UTC", phil.id, t0())
        .await
        .unwrap();

    store
        .log_event(kitchen.id, "task.completed", Some("watered"), Some(phil.id), t0())
        .await
        .unwrap();

    assert_eq!(store.recent_events(kitchen.id, 10).await.unwrap().len(), 1);
    assert_eq!(store.recent_events(office.id, 10).await.unwrap().len(), 0);
}
