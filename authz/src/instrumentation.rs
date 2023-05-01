use async_trait::async_trait;
use iox_time::{SystemProvider, TimeProvider};
use metric::{DurationHistogram, Metric, Registry};

use super::{Authorizer, Error, Permission};

const AUTHZ_DURATION_METRIC: &str = "authz_permissions_duration";

/// An instrumentation decorator over a [`Authorizer`] implementation.
///
/// This wrapper captures the latency distribution of the decorated
/// [`Authorizer::permissions()`] call, faceted by success/error result.
#[derive(Debug)]
pub struct AuthorizerInstrumentation<T, P = SystemProvider> {
    inner: T,
    time_provider: P,

    /// Permissions-check duration distribution for successes.
    ioxauth_rpc_duration_success: DurationHistogram,

    /// Permissions-check duration distribution for errors.
    ioxauth_rpc_duration_error: DurationHistogram,
}

impl<T> AuthorizerInstrumentation<T> {
    /// Record permissions-check duration metrics, broken down by result.
    pub fn new(registry: &Registry, inner: T) -> Self {
        let metric: Metric<DurationHistogram> =
            registry.register_metric(AUTHZ_DURATION_METRIC, "duration of authz permissions check");

        let ioxauth_rpc_duration_success = metric.recorder(&[("result", "success")]);
        let ioxauth_rpc_duration_error = metric.recorder(&[("result", "error")]);

        Self {
            inner,
            time_provider: Default::default(),
            ioxauth_rpc_duration_success,
            ioxauth_rpc_duration_error,
        }
    }
}

#[async_trait]
impl<T> Authorizer for AuthorizerInstrumentation<T>
where
    T: Authorizer,
{
    async fn permissions(
        &self,
        token: Option<Vec<u8>>,
        perms: &[Permission],
    ) -> Result<Vec<Permission>, Error> {
        let t = self.time_provider.now();
        let res = self.inner.permissions(token, perms).await;

        if let Some(delta) = self.time_provider.now().checked_duration_since(t) {
            match &res {
                Ok(_) => self.ioxauth_rpc_duration_success.record(delta),
                Err(_) => self.ioxauth_rpc_duration_error.record(delta),
            };
        }

        res
    }
}

#[cfg(test)]
mod test {
    use std::collections::VecDeque;

    use metric::{Attributes, Registry};
    use parking_lot::Mutex;

    use super::*;
    use crate::{Action, Resource};

    #[derive(Debug, Default)]
    struct MockAuthorizerState {
        ret: VecDeque<Result<Vec<Permission>, Error>>,
    }

    #[derive(Debug, Default)]
    struct MockAuthorizer {
        state: Mutex<MockAuthorizerState>,
    }

    impl MockAuthorizer {
        pub(crate) fn with_permissions_return(
            self,
            ret: impl Into<VecDeque<Result<Vec<Permission>, Error>>>,
        ) -> Self {
            self.state.lock().ret = ret.into();
            self
        }
    }

    #[async_trait]
    impl Authorizer for MockAuthorizer {
        async fn permissions(
            &self,
            _token: Option<Vec<u8>>,
            _perms: &[Permission],
        ) -> Result<Vec<Permission>, Error> {
            self.state
                .lock()
                .ret
                .pop_front()
                .expect("no mock sink value to return")
        }
    }

    #[allow(dead_code)]
    fn assert_metric_counts(metrics: &Registry, expected_success: u64, expected_err: u64) -> bool {
        let histogram = &metrics
            .get_instrument::<Metric<DurationHistogram>>(AUTHZ_DURATION_METRIC)
            .expect("failed to read metric");

        assert_eq!(
            histogram
                .get_observer(&Attributes::from(&[("result", "success"),]))
                .expect("failed to get observer")
                .fetch()
                .sample_count(),
            expected_success,
            "success counts did not match"
        );
        assert_eq!(
            histogram
                .get_observer(&Attributes::from(&[("result", "error"),]))
                .expect("failed to get observer")
                .fetch()
                .sample_count(),
            expected_err,
            "error counts did not match"
        );
        true
    }

    #[tokio::test]
    async fn test_authz_metric_record_for_all_exposed_interfaces() {
        let metrics = Registry::default();
        let decorated_authz = AuthorizerInstrumentation::new(
            &metrics,
            MockAuthorizer::default().with_permissions_return([
                Ok(vec![]),
                Ok(vec![Permission::ResourceAction(
                    Resource::Database("foo".to_string()),
                    Action::Write,
                )]),
            ]),
        );

        let any_token = Some(vec![]);

        let got = decorated_authz.permissions(any_token.clone(), &[]).await;
        assert!(got.is_ok());
        assert!(
            assert_metric_counts(&metrics, 1, 0),
            "Authorizer::permissions() calls are recorded"
        );

        let got = decorated_authz.require_any_permission(any_token, &[]).await;
        assert!(got.is_ok());
        assert!(
            assert_metric_counts(&metrics, 2, 0),
            "Authorizer::require_any_permission() calls are recorded"
        );
    }

    macro_rules! test_authorizer_metric {
        (
            $name:ident,
            $rpc_response:expr,
            $will_pass_auth:expr,
            $expected_success_cnt:expr,
            $expected_error_cnt:expr,
        ) => {
            paste::paste! {
                #[tokio::test]
                async fn [<test_authorizer_metric_ $name>]() {
                    let metrics = Registry::default();
                    let decorated_authz = AuthorizerInstrumentation::new(
                        &metrics,
                        MockAuthorizer::default().with_permissions_return([$rpc_response])
                    );

                    let token = "any".as_bytes().to_vec();
                    let got = decorated_authz
                        .require_any_permission(Some(token), &[])
                        .await;
                    assert_eq!(got.is_ok(), $will_pass_auth);
                    assert!(
                        assert_metric_counts(&metrics, $expected_success_cnt, $expected_error_cnt),
                        "rpc errors are recorded"
                    );
                }
            }
        };
    }

    test_authorizer_metric!(
        ok,
        Ok(vec![Permission::ResourceAction(
            Resource::Database("foo".to_string()),
            Action::Write,
        )]),
        true,
        1,
        0,
    );

    test_authorizer_metric!(
        will_record_failure_if_rpc_fails,
        Err(Error::verification("test", "test error")),
        false,
        0,
        1,
    );

    test_authorizer_metric!(
        will_record_success_if_rpc_pass_but_auth_fails,
        Ok(vec![]),
        false,
        1,
        0,
    );
}
