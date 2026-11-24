//! The least privilege this driver can be given.
//!
//! The AWS wizard shows the user the policy they are about to attach to an
//! IAM user, and a policy that is not the truth is worse than no policy at
//! all: too narrow and the first provision fails with an
//! `UnauthorizedOperation` nobody can act on, too wide and flyco asked for
//! rights it never uses. So the list is derived from what the driver
//! actually sends, and a test proves it:
//! [`tests::the_ec2_actions_are_exactly_the_ones_the_driver_calls`] reads
//! this crate's own source and refuses to pass if a call site names an EC2
//! action this module does not.
//!
//! Everything outside EC2 is derived rather than listed. Each of those
//! services is reached through exactly one JSON-RPC target, and the target
//! constant already carries the action name after its `.` — so
//! [`OTHER_SERVICES`] pairs a signing name with the constant and the action
//! is read off it. A driver that starts calling a second Cost Explorer
//! operation changes the constant, and the policy changes with it.

use crate::aws::{costs, fargate, identity, image, pricing, quotas};

/// IAM's service prefix for EC2.
pub const EC2: &str = "ec2";

/// IAM's service prefix for ECS.
pub const ECS: &str = fargate::SERVICE;

/// Every EC2 action the driver names in a request of its own.
///
/// In the order the driver reaches them: reading what an account can deploy,
/// reading live spot prices, finding or building the workspace network,
/// launching, addressing, and finally the lifecycle operations a session's
/// machine goes through.
pub const EC2_ACTIONS: [&str; 23] = [
    "DescribeRegions",
    "DescribeInstanceTypes",
    "DescribeInstanceTypeOfferings",
    "DescribeInstances",
    "DescribeSpotPriceHistory",
    "DescribeVpcs",
    "DescribeSubnets",
    "DescribeSecurityGroups",
    "CreateSecurityGroup",
    "AuthorizeSecurityGroupIngress",
    "DescribeImages",
    "RunInstances",
    "DescribeAddresses",
    "DescribeVolumes",
    "AllocateAddress",
    "AssociateAddress",
    "StartInstances",
    "StopInstances",
    "ModifyInstanceAttribute",
    "CancelSpotInstanceRequests",
    "TerminateInstances",
    "ReleaseAddress",
    "DeleteVolume",
];

/// EC2 actions no call site names and every provision needs anyway.
///
/// `RunInstances` and `AllocateAddress` both carry `TagSpecification`, and
/// EC2 authorises the tags in one as a separate `ec2:CreateTags` — a launch
/// without it fails with `UnauthorizedOperation` naming an action the driver
/// never sent. Listed apart from [`EC2_ACTIONS`] so the test above can hold
/// that list to exactly the call sites, and so the reason a right is granted
/// is written down beside it.
pub const IMPLIED_EC2_ACTIONS: [&str; 1] = ["CreateTags"];

/// Every ECS action the Fargate path names in a request of its own.
pub const ECS_ACTIONS: [&str; 11] = [
    "DescribeClusters",
    "CreateCluster",
    "RegisterTaskDefinition",
    "ListTaskDefinitions",
    "DescribeTaskDefinition",
    "DeregisterTaskDefinition",
    "DeleteTaskDefinitions",
    "ListTasks",
    "DescribeTasks",
    "RunTask",
    "StopTask",
];

/// ECS actions no call site names directly and Fargate provisions need anyway.
///
/// `RegisterTaskDefinition` passes `tags` and `RunTask` propagates them to
/// tasks, which IAM authorizes through `ecs:TagResource`.
pub const IMPLIED_ECS_ACTIONS: [&str; 1] = ["TagResource"];

/// The services reached through one JSON-RPC target each, and that target.
///
/// The action is the segment after the target's final `.`, which is how the
/// AWS wire protocol spells it and how IAM spells it — so nothing here is
/// written twice.
pub const OTHER_SERVICES: [(&str, &str); 4] = [
    (costs::SERVICE, costs::TARGET),
    (pricing::SERVICE, pricing::GET_PRODUCTS_TARGET),
    (quotas::SERVICE, quotas::LIST_TARGET),
    (image::SERVICE, image::TARGET),
];

/// Every action the minimal policy grants, `service:Action`, sorted.
///
/// Sorted because the policy is a document a user reads and diffs, and an
/// order that moved with the source layout would make every regeneration
/// look like a change.
#[must_use]
pub fn actions() -> Vec<String> {
    let mut actions: Vec<String> = EC2_ACTIONS
        .iter()
        .chain(&IMPLIED_EC2_ACTIONS)
        .map(|action| format!("{EC2}:{action}"))
        .chain(
            ECS_ACTIONS
                .iter()
                .chain(&IMPLIED_ECS_ACTIONS)
                .map(|action| format!("{ECS}:{action}")),
        )
        .chain(
            OTHER_SERVICES
                .iter()
                .map(|(service, target)| format!("{service}:{}", operation(target))),
        )
        // `sts:GetCallerIdentity` is the one call made through the Query
        // protocol rather than JSON-RPC, because STS answers in XML.
        .chain(core::iter::once(format!(
            "{}:{}",
            identity::SERVICE,
            identity::ACTION
        )))
        .collect();
    actions.sort_unstable();
    actions
}

/// The operation half of an `X-Amz-Target`, which is `Service.Operation`.
fn operation(target: &str) -> &str {
    target.rsplit('.').next().unwrap_or(target)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::{
        EC2_ACTIONS, ECS_ACTIONS, IMPLIED_EC2_ACTIONS, IMPLIED_ECS_ACTIONS, actions, operation,
    };

    /// The driver's own sources, read at compile time.
    ///
    /// Checking `mod.rs`, `pricing.rs`, and `fargate.rs` keeps the IAM policy
    /// locked to every EC2 and ECS action the crate actually sends.
    const DRIVER: &str = include_str!("mod.rs");
    const PRICING: &str = include_str!("pricing.rs");
    const FARGATE: &str = include_str!("fargate.rs");

    /// The first argument of every EC2 call site, in every spelling the
    /// driver uses.
    ///
    /// Whitespace-stripped, because the calls are formatted across four
    /// lines as often as one. A call site whose region argument is spelled
    /// some other way is invisible to the scan below and its action then
    /// shows up as an `EC2_ACTIONS` entry nothing calls — which fails the
    /// test rather than silently under-reading the driver.
    const REGION_ARGUMENTS: [&str; 3] = ["region,", "&region,", "identity::REGION,"];

    /// Extracts the first string literal argument after an opener when the
    /// prefix before that literal matches `predicate`.
    fn extract_literals(
        source: &str,
        opener: &str,
        predicate: impl Fn(&str) -> bool,
    ) -> BTreeSet<String> {
        let mut found = BTreeSet::new();
        for site in source.split(opener).skip(1) {
            let Some(open) = site.find('"') else { continue };
            let Some(close) = site[open + 1..].find('"') else {
                continue;
            };
            let between: String = site[..open].split_whitespace().collect();
            if !predicate(&between) {
                continue;
            }
            found.insert(site[open + 1..open + 1 + close].to_owned());
        }
        found
    }

    /// Every EC2 action literal sent through `.ec2(`, `.ec2_call(`, or
    /// `send_ec2(`.
    fn called_ec2() -> BTreeSet<String> {
        let mut found = BTreeSet::new();
        for opener in [".ec2(", ".ec2_call("] {
            found.extend(extract_literals(DRIVER, opener, |between| {
                REGION_ARGUMENTS.contains(&between)
            }));
        }
        found.extend(extract_literals(PRICING, "send_ec2(", |between| {
            between.ends_with("region,")
        }));
        found
    }

    /// Every ECS action literal sent through `.ecs(` in `fargate.rs`.
    fn called_ecs() -> BTreeSet<String> {
        let mut found = BTreeSet::new();
        for opener in [
            ".ecs(",
            ".ecs::<_,DefinitionAnswer>(",
            ".ecs::<_,TaskDefinitionDeletions>(",
        ] {
            found.extend(extract_literals(FARGATE, opener, |between| {
                between == "region," || between == "&region,"
            }));
        }
        // Also scan `.ecs::<...>(region, "Action", ...)` with whitespace intact.
        for site in FARGATE.split(".ecs::<").skip(1) {
            let Some(paren) = site.find('(') else {
                continue;
            };
            let rest = &site[paren + 1..];
            let Some(open) = rest.find('"') else { continue };
            let Some(close) = rest[open + 1..].find('"') else {
                continue;
            };
            let between: String = rest[..open].split_whitespace().collect();
            if between == "region," || between == "&region," {
                found.insert(rest[open + 1..open + 1 + close].to_owned());
            }
        }
        found
    }

    #[test]
    fn the_ec2_actions_are_exactly_the_ones_the_driver_calls() {
        let listed: BTreeSet<String> = EC2_ACTIONS.iter().map(|&a| a.to_owned()).collect();
        assert_eq!(
            called_ec2(),
            listed,
            "EC2_ACTIONS must name every EC2 action the driver and pricing module send, and no others"
        );
    }

    #[test]
    fn the_ecs_actions_are_exactly_the_ones_fargate_calls() {
        let listed: BTreeSet<String> = ECS_ACTIONS.iter().map(|&a| a.to_owned()).collect();
        assert_eq!(
            called_ecs(),
            listed,
            "ECS_ACTIONS must name every ECS action fargate.rs sends, and no others"
        );
    }

    #[test]
    fn an_implied_action_is_never_also_a_call_site() {
        for implied in IMPLIED_EC2_ACTIONS {
            assert!(
                !EC2_ACTIONS.contains(&implied),
                "{implied} is granted as an implied right and also listed as a call site"
            );
        }
        for implied in IMPLIED_ECS_ACTIONS {
            assert!(
                !ECS_ACTIONS.contains(&implied),
                "{implied} is granted as an implied right and also listed as a call site"
            );
        }
    }

    #[test]
    fn the_policy_names_every_service_the_driver_signs_for() {
        let actions = actions();
        for expected in [
            "ec2:RunInstances",
            "ec2:CreateTags",
            "ec2:CancelSpotInstanceRequests",
            "ec2:DescribeSpotPriceHistory",
            "ecs:RunTask",
            "ecs:RegisterTaskDefinition",
            "ecs:DeleteTaskDefinitions",
            "ecs:TagResource",
            "sts:GetCallerIdentity",
            "ce:GetCostAndUsage",
            "pricing:GetProducts",
            "servicequotas:ListServiceQuotas",
            "ssm:GetParameter",
        ] {
            assert!(actions.contains(&expected.to_owned()), "missing {expected}");
        }
        assert_eq!(
            actions.len(),
            EC2_ACTIONS.len()
                + IMPLIED_EC2_ACTIONS.len()
                + ECS_ACTIONS.len()
                + IMPLIED_ECS_ACTIONS.len()
                + 5
        );
    }

    #[test]
    fn the_policy_is_sorted_and_free_of_duplicates() {
        let actions = actions();
        let unique: BTreeSet<&String> = actions.iter().collect();
        assert_eq!(unique.len(), actions.len());
        assert!(actions.windows(2).all(|pair| pair[0] < pair[1]));
    }

    #[test]
    fn an_operation_is_the_tail_of_its_target() {
        assert_eq!(
            operation("AWSInsightsIndexService.GetCostAndUsage"),
            "GetCostAndUsage"
        );
        assert_eq!(operation("GetProducts"), "GetProducts");
    }
}
