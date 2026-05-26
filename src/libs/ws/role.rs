use std::collections::HashSet;
use std::hash::Hash;

/// Dynamic role checker - converts stored roles and checks against endpoint's allowed set.
pub trait RoleChecker<R>: Send + Sync {
    fn check(&self, stored_roles: &[R]) -> bool;
}

/// Checker that converts StoredRole to EndpointRole via Into trait.
/// Used when connection stores one role type but endpoint requires another.
pub struct IntoRoleChecker<S, E>
where
    E: Eq + Hash + Send + Sync,
    S: Send + Sync,
{
    allowed: HashSet<E>,
    _phantom: std::marker::PhantomData<S>,
}

impl<S, E> IntoRoleChecker<S, E>
where
    E: Eq + Hash + Send + Sync,
    S: Send + Sync,
{
    pub fn new(allowed: HashSet<E>) -> Self {
        Self {
            allowed,
            _phantom: std::marker::PhantomData,
        }
    }
}

impl<S, E> RoleChecker<S> for IntoRoleChecker<S, E>
where
    S: Clone + Eq + Hash + Into<E> + Send + Sync,
    E: Eq + Hash + Send + Sync,
{
    fn check(&self, stored_roles: &[S]) -> bool {
        if self.allowed.is_empty() || stored_roles.is_empty() {
            return false;
        }
        stored_roles
            .iter()
            .any(|s| self.allowed.contains(&s.clone().into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, PartialEq, Eq, Hash, Debug)]
    enum TestRole1 {
        Admin,
        User,
    }

    #[derive(Clone, PartialEq, Eq, Hash, Debug)]
    enum TestRole2 {
        Owner,
        Member,
    }

    impl From<TestRole2> for TestRole1 {
        fn from(r: TestRole2) -> TestRole1 {
            match r {
                TestRole2::Owner => TestRole1::Admin,
                TestRole2::Member => TestRole1::User,
            }
        }
    }

    #[test]
    fn into_role_checker_works() {
        let allowed: HashSet<TestRole1> = [TestRole1::Admin].into_iter().collect();
        let checker = IntoRoleChecker::<TestRole2, TestRole1>::new(allowed);

        assert!(checker.check(&[TestRole2::Owner]));

        // Member maps to User - should fail (only Admin allowed)
        assert!(!checker.check(&[TestRole2::Member]));

        assert!(checker.check(&[TestRole2::Member, TestRole2::Owner]));
    }

    #[test]
    fn empty_roles_fails() {
        let allowed: HashSet<TestRole1> = [TestRole1::Admin].into_iter().collect();
        let checker = IntoRoleChecker::<TestRole2, TestRole1>::new(allowed);

        assert!(!checker.check(&[]));
    }

    #[test]
    fn empty_allowed_fails() {
        let allowed: HashSet<TestRole1> = HashSet::new();
        let checker = IntoRoleChecker::<TestRole2, TestRole1>::new(allowed);

        assert!(!checker.check(&[TestRole2::Owner]));
    }
}
