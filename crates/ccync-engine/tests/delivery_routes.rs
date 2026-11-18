use ccync_engine::delivery::{
    plan_delivery, plan_delivery_with_commands, prepare_delivery,
    prepare_delivery_with_native_routes, ComponentBodies, ComponentRootPair, ComponentRouteChoice,
    DeliveryComponent, DeliveryInventory, ForeignComponent, PendingReference,
    UnconvertibleCommands, SHARED_ROOT_CONSUMERS,
};
use ccync_engine::delivery::{
    plan_final_group_routes, plan_native_routes, DeliveryPlan, NativeAdapterState, NativeAdapters,
    NativeRouteStatus, PhysicalIdentity,
};
use ccync_engine::managed_view::native_status_note;
use ccync_foundation::provider::{
    ClientFaceSupportDisposition, ComponentIdentity, ComponentKind, ProviderSourceKey, TargetAlias,
};
use ccync_foundation::target::{ClientFace, ProjectionTargetId as Target};

#[test]
fn face_capabilities_distinguish_copilot_and_all_agy_faces() {
    let capability = |root: &str, face: ClientFace| {
        SHARED_ROOT_CONSUMERS
            .iter()
            .find(|row| row.root == root)
            .and_then(|row| {
                row.face_support
                    .iter()
                    .find(|(supported_face, _)| *supported_face == face)
                    .map(|(_, support)| *support)
            })
            .unwrap_or(ClientFaceSupportDisposition::Unsupported)
    };

    let agents = "~/.agents/skills";
    assert_eq!(
        capability(agents, ClientFace::CopilotCli),
        ClientFaceSupportDisposition::Supported
    );
    assert_eq!(
        capability(agents, ClientFace::CopilotVsCode),
        ClientFaceSupportDisposition::Unsupported
    );
    assert_eq!(
        capability("~/.gemini/config/agents", ClientFace::AgyCli),
        ClientFaceSupportDisposition::Supported
    );
    assert_eq!(
        capability("~/.gemini/config/skills", ClientFace::AgyCli),
        ClientFaceSupportDisposition::Unsupported
    );
    assert_eq!(
        capability("~/.gemini/config/skills", ClientFace::AgyIde),
        ClientFaceSupportDisposition::Supported
    );
    assert_eq!(
        capability("~/.gemini/config/agents", ClientFace::AgyIde),
        ClientFaceSupportDisposition::Unsupported
    );
    assert_eq!(
        capability("~/.gemini/config/agents", ClientFace::AgyGui),
        ClientFaceSupportDisposition::Supported
    );
    assert_eq!(
        capability("~/.gemini/config/skills", ClientFace::AgyGui),
        ClientFaceSupportDisposition::Supported
    );

    let agent_skills = SHARED_ROOT_CONSUMERS
        .iter()
        .find(|row| row.root == agents)
        .unwrap();
    assert_eq!(
        agent_skills.component_kinds,
        &[ComponentKind::Skill, ComponentKind::Command]
    );
    assert_eq!(agent_skills.scope, "user");
    assert_eq!(
        agent_skills.served_faces,
        &[
            ClientFace::Codex,
            ClientFace::CopilotCli,
            ClientFace::OpenCode
        ]
    );
    assert!(agent_skills.incidental_readers.is_empty());

    let plan = plan_delivery(
        &DeliveryInventory {
            components: vec![component(identity(
                "example",
                "plugin",
                ComponentKind::Skill,
                "existing",
            ))],
            foreign_components: Vec::new(),
        },
        &[Target::Codex, Target::Copilot, Target::Opencode],
        &[],
    );
    assert_eq!(
        shared_root_consumers(&plan, agents),
        vec![Target::Codex, Target::Copilot, Target::Opencode]
    );
}

fn identity(source: &str, plugin: &str, kind: ComponentKind, id: &str) -> ComponentIdentity {
    ComponentIdentity::new(
        ProviderSourceKey::new(source).unwrap(),
        plugin,
        None,
        kind,
        id,
    )
    .unwrap()
}

fn component(identity: ComponentIdentity) -> DeliveryComponent {
    DeliveryComponent {
        identity,
        provider_hooks_declared: false,
        enabled: true,
        references_preserved: true,
    }
}

fn route(
    plan: &ccync_engine::delivery::DeliveryPlan,
    target: Target,
) -> &ccync_engine::delivery::ComponentTargetRoute {
    plan.routes
        .iter()
        .find(|route| route.target == target)
        .unwrap()
}

#[test]
fn delivery_routes_codex_and_opencode_use_one_shared_copy() {
    let identity = identity("provider", "provider", ComponentKind::Skill, "review");
    let plan = plan_delivery(
        &DeliveryInventory {
            components: vec![component(identity.clone())],
            foreign_components: vec![],
        },
        &[Target::Codex, Target::Opencode],
        &[],
    );

    assert_eq!(plan.shared_roots.len(), 1);
    assert_eq!(plan.shared_roots[0].root, "~/.agents/skills");
    assert_eq!(plan.shared_roots[0].components, vec![identity]);
    assert_eq!(
        plan.shared_roots[0].reached_consumers,
        vec![Target::Codex, Target::Copilot, Target::Opencode]
    );
    assert!(
        matches!(route(&plan, Target::Opencode).choice, ComponentRouteChoice::ServedByRoot { ref root } if root == "~/.agents/skills")
    );
}

#[test]
fn delivery_routes_three_targets_report_opencode_multi_root_discovery() {
    let identity = identity("provider", "provider", ComponentKind::Skill, "review");
    let plan = plan_delivery(
        &DeliveryInventory {
            components: vec![component(identity)],
            foreign_components: vec![],
        },
        &[Target::Claude, Target::Codex, Target::Opencode],
        &[],
    );

    assert_eq!(plan.shared_roots.len(), 2);
    let opencode = route(&plan, Target::Opencode);
    assert!(matches!(
        opencode.choice,
        ComponentRouteChoice::ServedByRoot { .. }
    ));
    assert!(opencode.multi_root);
    assert_eq!(opencode.discovered_root_count, 2);
}

#[test]
fn delivery_routes_copilot_is_reached_but_gets_private_copy() {
    let identity = identity("provider", "provider", ComponentKind::Skill, "review");
    let plan = plan_delivery(
        &DeliveryInventory {
            components: vec![component(identity)],
            foreign_components: vec![],
        },
        &[Target::Codex, Target::Copilot],
        &[],
    );

    let copilot = route(&plan, Target::Copilot);
    assert!(
        matches!(copilot.choice, ComponentRouteChoice::Direct { ref root } if root == "~/.copilot/skills")
    );
    assert!(copilot.multi_root);
    assert_eq!(copilot.discovered_root_count, 2);
    let shared = plan
        .shared_roots
        .iter()
        .find(|root| root.root == "~/.agents/skills")
        .unwrap();
    assert!(shared.reached_consumers.contains(&Target::Copilot));
    assert!(!SHARED_ROOT_CONSUMERS
        .iter()
        .find(|row| row.root == "~/.agents/skills")
        .unwrap()
        .served_by
        .contains(&Target::Copilot));
}

#[test]
fn delivery_routes_hooks_are_reported_for_each_target_and_other_components_route() {
    let mut skill = component(identity(
        "provider",
        "hooked-provider",
        ComponentKind::Skill,
        "review",
    ));
    skill.provider_hooks_declared = true;
    let hook = component(identity(
        "provider",
        "hooked-provider",
        ComponentKind::Hook,
        "before-tool",
    ));
    let plan = plan_delivery(
        &DeliveryInventory {
            components: vec![skill, hook],
            foreign_components: vec![],
        },
        &[Target::Claude, Target::Codex],
        &[],
    );

    for target in [Target::Claude, Target::Codex] {
        let unsupported = plan
            .routes
            .iter()
            .find(|item| item.target == target && item.identity.kind == ComponentKind::Hook)
            .unwrap();
        assert!(matches!(
            unsupported.choice,
            ComponentRouteChoice::Unsupported { .. }
        ));
        assert_eq!(
            unsupported.unsupported_hooks_provider.as_deref(),
            Some("hooked-provider")
        );
    }
    assert!(matches!(
            plan.routes
                .iter()
                .find(|item| item.target == Target::Claude
                    && item.identity.kind == ComponentKind::Skill)
                .unwrap()
                .choice,
            ComponentRouteChoice::ServedByRoot { .. }
        ));
}

#[test]
fn delivery_routes_foreign_collision_uses_provider_alias_and_records_path() {
    let provider = identity("provider", "provider", ComponentKind::Skill, "review");
    let foreign = identity("foreign", "foreign", ComponentKind::Skill, "review");
    let plan = plan_delivery(
        &DeliveryInventory {
            components: vec![component(provider)],
            foreign_components: vec![ForeignComponent {
                identity: foreign,
                path: "~/.claude/skills/review".into(),
            }],
        },
        &[Target::Claude],
        &[],
    );

    let route = route(&plan, Target::Claude);
    assert_ne!(route.alias.as_deref(), Some("review"));
    assert_eq!(
        route.foreign_alias_path.as_deref(),
        Some("~/.claude/skills/review")
    );
}

#[test]
fn delivery_routes_unpreservable_references_block_the_unit() {
    let mut component = component(identity(
        "provider",
        "provider",
        ComponentKind::Skill,
        "review",
    ));
    component.references_preserved = false;
    let plan = plan_delivery(
        &DeliveryInventory {
            components: vec![component],
            foreign_components: vec![],
        },
        &[Target::Claude],
        &[],
    );

    assert!(matches!(
        route(&plan, Target::Claude).choice,
        ComponentRouteChoice::Blocked { .. }
    ));
}

#[test]
fn delivery_routes_alias_planning_collision_blocks_the_unit() {
    let provider = identity("provider", "provider", ComponentKind::Skill, "review");
    let foreign = identity("foreign", "foreign", ComponentKind::Skill, "review");
    let plan = plan_delivery(
        &DeliveryInventory {
            components: vec![component(provider)],
            foreign_components: vec![
                ForeignComponent {
                    identity: foreign.clone(),
                    path: "~/.claude/skills/review".into(),
                },
                ForeignComponent {
                    identity: foreign,
                    path: "~/.config/opencode/skills/review".into(),
                },
            ],
        },
        &[Target::Claude],
        &[],
    );

    assert!(matches!(
        route(&plan, Target::Claude).choice,
        ComponentRouteChoice::Blocked { .. }
    ));
}

#[test]
fn delivery_routes_disabled_component_receives_no_publication_route() {
    let mut item = component(identity(
        "provider",
        "provider",
        ComponentKind::Skill,
        "review",
    ));
    item.enabled = false;
    let plan = plan_delivery(
        &DeliveryInventory {
            components: vec![item],
            foreign_components: vec![],
        },
        &[Target::Claude, Target::Codex, Target::Opencode],
        &[],
    );

    assert!(plan.shared_roots.is_empty());
    for target in [Target::Claude, Target::Codex, Target::Opencode] {
        let r = route(&plan, target);
        assert_eq!(r.choice, ComponentRouteChoice::Disabled);
    }
}

#[test]
fn delivery_routes_agy_skills_and_agents_use_shared_roots_and_ide_reports_unsupported_agent() {
    let skill = component(identity(
        "provider",
        "provider",
        ComponentKind::Skill,
        "review",
    ));
    let agent = component(identity(
        "provider",
        "provider",
        ComponentKind::Agent,
        "reviewer",
    ));
    let plan = plan_delivery(
        &DeliveryInventory {
            components: vec![skill.clone(), agent.clone()],
            foreign_components: vec![],
        },
        &[Target::AgyCli, Target::AgyIde, Target::AgyGui],
        &[],
    );

    // Skill shared root: ~/.gemini/config/skills reaches AgyIde and AgyGui
    let skill_shared = plan
        .shared_roots
        .iter()
        .find(|r| r.root == "~/.gemini/config/skills")
        .expect("agy skills shared root exists");
    assert_eq!(skill_shared.components, vec![skill.identity.clone()]);
    assert_eq!(
        skill_shared.reached_consumers,
        vec![Target::AgyIde, Target::AgyGui]
    );

    // AgyCli has private skills root
    let agy_cli_skill = plan
        .routes
        .iter()
        .find(|r| r.target == Target::AgyCli && r.identity == skill.identity)
        .unwrap();
    assert_eq!(
        agy_cli_skill.choice,
        ComponentRouteChoice::Direct {
            root: "~/.gemini/antigravity-cli/skills".into()
        }
    );

    // AgyIde and AgyGui are served by ~/.gemini/config/skills
    for target in [Target::AgyIde, Target::AgyGui] {
        let r = plan
            .routes
            .iter()
            .find(|r| r.target == target && r.identity == skill.identity)
            .unwrap();
        assert_eq!(
            r.choice,
            ComponentRouteChoice::ServedByRoot {
                root: "~/.gemini/config/skills".into()
            }
        );
    }

    // Agent shared root: ~/.gemini/config/agents reaches AgyCli and AgyGui
    let agent_shared = plan
        .shared_roots
        .iter()
        .find(|r| r.root == "~/.gemini/config/agents")
        .expect("agy agents shared root exists");
    assert_eq!(agent_shared.components, vec![agent.identity.clone()]);
    assert_eq!(
        agent_shared.reached_consumers,
        vec![Target::AgyCli, Target::AgyGui]
    );

    // AgyCli and AgyGui are served by ~/.gemini/config/agents
    for target in [Target::AgyCli, Target::AgyGui] {
        let r = plan
            .routes
            .iter()
            .find(|r| r.target == target && r.identity == agent.identity)
            .unwrap();
        assert_eq!(
            r.choice,
            ComponentRouteChoice::ServedByRoot {
                root: "~/.gemini/config/agents".into()
            }
        );
    }

    // AgyIde has no agent surface -> Unsupported
    let agy_ide_agent = plan
        .routes
        .iter()
        .find(|r| r.target == Target::AgyIde && r.identity == agent.identity)
        .unwrap();
    assert!(matches!(
        agy_ide_agent.choice,
        ComponentRouteChoice::Unsupported { .. }
    ));
}

#[test]
fn delivery_routes_shared_root_table_conforms_to_approach_spec() {
    assert_eq!(SHARED_ROOT_CONSUMERS.len(), 4);
    for row in SHARED_ROOT_CONSUMERS {
        assert!(!row.root.is_empty());
        assert!(!row.consumers.is_empty());
        assert!(!row.sources.is_empty());
        assert_eq!(row.checked, "2026-10-16");
    }

    let agents_skills = SHARED_ROOT_CONSUMERS
        .iter()
        .find(|r| r.root == "~/.agents/skills")
        .unwrap();
    assert_eq!(
        agents_skills.consumers,
        &[Target::Codex, Target::Copilot, Target::Opencode]
    );
    assert_eq!(agents_skills.served_by, &[Target::Codex, Target::Opencode]);
    assert!(agents_skills
        .sources
        .contains(&"https://learn.chatgpt.com/docs/build-skills"));
    assert!(agents_skills
        .sources
        .contains(&"https://docs.github.com/en/copilot/concepts/agents/about-agent-skills"));
    assert!(agents_skills
        .sources
        .contains(&"https://opencode.ai/docs/skills/"));
}

#[test]
fn delivery_routes_commands_route_natively_or_into_the_skill_root_as_command_to_skill() {
    let cmd = identity("provider", "provider", ComponentKind::Command, "build");
    let plan = plan_delivery(
        &DeliveryInventory {
            components: vec![component(cmd.clone())],
            foreign_components: vec![],
        },
        &[
            Target::Claude,
            Target::Codex,
            Target::Copilot,
            Target::Opencode,
            Target::AgyCli,
        ],
        &[],
    );

    // Native command roots stay direct and are not command-to-skill routes.
    let claude = route(&plan, Target::Claude);
    assert_eq!(
        claude.choice,
        ComponentRouteChoice::Direct {
            root: "~/.claude/commands".into()
        }
    );
    assert!(!claude.command_to_skill);
    let opencode = route(&plan, Target::Opencode);
    assert_eq!(
        opencode.choice,
        ComponentRouteChoice::Direct {
            root: "~/.config/opencode/commands".into()
        }
    );
    assert!(!opencode.command_to_skill);
    // OpenCode also discovers the converted copy in `~/.agents/skills`.
    assert!(opencode.multi_root);
    assert_eq!(opencode.discovered_root_count, 2);

    // Codex is served by the shared skill root that holds the converted copy.
    let codex = route(&plan, Target::Codex);
    assert_eq!(
        codex.choice,
        ComponentRouteChoice::ServedByRoot {
            root: "~/.agents/skills".into()
        }
    );
    assert!(codex.command_to_skill);

    // Copilot is reached by that root but not served, so it gets a private copy.
    let copilot = route(&plan, Target::Copilot);
    assert_eq!(
        copilot.choice,
        ComponentRouteChoice::Direct {
            root: "~/.copilot/skills".into()
        }
    );
    assert!(copilot.command_to_skill);
    assert!(copilot.multi_root);

    // The converted command is written once into the shared skill root.
    let shared = plan
        .shared_roots
        .iter()
        .find(|root| root.root == "~/.agents/skills")
        .expect("the converted command is planned into the shared skill root");
    assert_eq!(shared.components, vec![cmd]);

    // AGY CLI has no command-to-skill surface: unsupported and reported.
    let agy = route(&plan, Target::AgyCli);
    assert!(
        matches!(agy.choice, ComponentRouteChoice::Unsupported { ref reason } if reason.contains("command-to-skill")),
        "{:?}",
        agy.choice
    );
    assert!(!agy.command_to_skill);
}

#[test]
fn delivery_routes_unconvertible_command_stays_unsupported_without_a_skill_root_copy() {
    let cmd = identity("provider", "provider", ComponentKind::Command, "build");
    let unconvertible = UnconvertibleCommands::from([(
        cmd.clone(),
        "positional command arguments are not equivalent to skill arguments".to_string(),
    )]);
    let plan = plan_delivery_with_commands(
        &DeliveryInventory {
            components: vec![component(cmd)],
            foreign_components: vec![],
        },
        &[
            Target::Claude,
            Target::Codex,
            Target::Copilot,
            Target::Opencode,
        ],
        &[],
        &unconvertible,
    );

    for target in [Target::Codex, Target::Copilot] {
        let r = route(&plan, target);
        assert!(
            matches!(r.choice, ComponentRouteChoice::Unsupported { ref reason } if reason.contains("positional command arguments")),
            "{target}: {:?}",
            r.choice
        );
        assert!(!r.command_to_skill);
    }
    assert!(plan.shared_roots.is_empty(), "{:?}", plan.shared_roots);
    let opencode = route(&plan, Target::Opencode);
    assert!(matches!(
        opencode.choice,
        ComponentRouteChoice::Direct { .. }
    ));
    assert!(!opencode.multi_root);
}

#[test]
fn delivery_routes_command_to_skill_shares_the_skill_root_with_skills() {
    let skill = identity("provider", "provider", ComponentKind::Skill, "review");
    let cmd = identity("provider", "provider", ComponentKind::Command, "build");
    let plan = plan_delivery(
        &DeliveryInventory {
            components: vec![component(skill.clone()), component(cmd.clone())],
            foreign_components: vec![],
        },
        &[Target::Codex, Target::Opencode],
        &[],
    );

    let shared = plan
        .shared_roots
        .iter()
        .find(|root| root.root == "~/.agents/skills")
        .unwrap();
    assert_eq!(shared.components, vec![skill, cmd.clone()]);
    let codex_command = plan
        .routes
        .iter()
        .find(|r| r.target == Target::Codex && r.identity == cmd)
        .unwrap();
    assert!(codex_command.command_to_skill);
    assert!(!codex_command.multi_root);
}

#[test]
fn delivery_routes_foreign_same_name_entry_in_the_skill_root_blocks_command_to_skill() {
    let cmd = identity("provider", "provider", ComponentKind::Command, "build");
    let foreign = identity(
        "native-foreign",
        "native-foreign",
        ComponentKind::Command,
        "build",
    );
    let plan = plan_delivery(
        &DeliveryInventory {
            components: vec![component(cmd)],
            foreign_components: vec![ForeignComponent {
                identity: foreign,
                path: "/home/user/.agents/skills/build".into(),
            }],
        },
        &[Target::Codex, Target::Opencode],
        &[],
    );

    let codex = route(&plan, Target::Codex);
    assert!(
        matches!(codex.choice, ComponentRouteChoice::Blocked { ref reason } if reason.contains("keeps the name 'build'")),
        "{:?}",
        codex.choice
    );
    assert!(codex.command_to_skill);
    assert_eq!(
        codex.foreign_alias_path.as_deref(),
        Some("/home/user/.agents/skills/build")
    );
    // A blocked conversion is never written, so the shared root holds nothing
    // and OpenCode does not count it as a second discovery root.
    assert!(plan.shared_roots.is_empty(), "{:?}", plan.shared_roots);
    assert!(!route(&plan, Target::Opencode).multi_root);
}

/// The prepared route for one component on one target.
fn prepared_route<'a>(
    plan: &'a ccync_engine::delivery::DeliveryPlan,
    identity: &ComponentIdentity,
    target: Target,
) -> &'a ccync_engine::delivery::ComponentTargetRoute {
    plan.routes
        .iter()
        .find(|route| route.identity == *identity && route.target == target)
        .unwrap()
}

fn prepared_route_mut<'a>(
    plan: &'a mut DeliveryPlan,
    identity: &ComponentIdentity,
    target: Target,
) -> &'a mut ccync_engine::delivery::ComponentTargetRoute {
    plan.routes
        .iter_mut()
        .find(|route| route.identity == *identity && route.target == target)
        .unwrap()
}

#[test]
fn prepared_blocked_consumer_vetoes_shared_root_and_reroutes_opencode() {
    // Claude rejects a plugin name that two sources supply, so every Claude
    // route blocks. Opencode would otherwise reach the Claude root through its
    // phase-one contribution.
    let review = identity("provider", "shared-plugin", ComponentKind::Skill, "review");
    let lint = identity("other", "shared-plugin", ComponentKind::Skill, "lint");
    let prepared = prepare_delivery(
        &DeliveryInventory {
            components: vec![component(review.clone()), component(lint.clone())],
            foreign_components: vec![],
        },
        &[Target::Claude, Target::Opencode],
        &[],
        &UnconvertibleCommands::new(),
        &ComponentBodies::new(),
    );

    assert!(matches!(
        prepared_route(&prepared.plan, &review, Target::Claude).choice,
        ComponentRouteChoice::Blocked { .. }
    ));
    assert_eq!(
        prepared_route(&prepared.plan, &review, Target::Opencode).choice,
        ComponentRouteChoice::Direct {
            root: "~/.config/opencode/skills".into()
        }
    );
    assert!(prepared.pairs.contains(&ComponentRootPair {
        identity: review.clone(),
        root: "~/.config/opencode/skills".into(),
    }));
    assert!(
        prepared
            .pairs
            .iter()
            .all(|pair| pair.root != "~/.claude/skills"),
        "{:?}",
        prepared.pairs
    );
    assert!(prepared
        .plan
        .shared_roots
        .iter()
        .all(|root| root.root != "~/.claude/skills"));

    let withheld = prepared
        .withheld
        .iter()
        .find(|item| item.pair.identity == review)
        .unwrap();
    assert_eq!(withheld.pair.root, "~/.claude/skills");
    assert_eq!(withheld.blocked_consumers, vec![Target::Claude]);
    assert_eq!(withheld.consumers, vec![Target::Claude, Target::Opencode]);
}

#[test]
fn prepared_ambiguous_reference_blocks_its_whole_unit_and_publishes_no_member() {
    let helper = identity("provider", "plugin", ComponentKind::Agent, "helper");
    let quiet = identity("provider", "plugin", ComponentKind::Agent, "quiet");
    let tidy = identity("clean", "clean", ComponentKind::Agent, "tidy");
    let inventory = DeliveryInventory {
        components: vec![
            component(helper.clone()),
            component(quiet.clone()),
            component(tidy.clone()),
            // Two provider skills named `dup` make `/dup` ambiguous for every
            // agent. Foreign entries never take part in reference resolution.
            component(identity("a", "plugin-a", ComponentKind::Skill, "dup")),
            component(identity("b", "plugin-b", ComponentKind::Skill, "dup")),
        ],
        foreign_components: vec![],
    };
    let mut bodies = ComponentBodies::new();
    bodies.insert(helper.clone(), Ok("Run /dup first.\n".into()));
    bodies.insert(quiet.clone(), Ok("Plain text, no references.\n".into()));
    bodies.insert(tidy.clone(), Ok("Plain text.\n".into()));

    let prepared = prepare_delivery(
        &inventory,
        &[Target::Claude],
        &[],
        &UnconvertibleCommands::new(),
        &bodies,
    );

    for blocked in [&helper, &quiet] {
        let choice = &prepared_route(&prepared.plan, blocked, Target::Claude).choice;
        assert!(
            matches!(choice, ComponentRouteChoice::Blocked { reason } if reason.contains("cannot be rewritten")),
            "{choice:?}"
        );
    }
    assert_eq!(
        prepared_route(&prepared.plan, &tidy, Target::Claude).choice,
        ComponentRouteChoice::Direct {
            root: "~/.claude/agents".into()
        }
    );
    let published = prepared
        .payloads
        .iter()
        .map(|payload| payload.identity.clone())
        .collect::<Vec<_>>();
    assert_eq!(published, vec![tidy.clone()]);
    assert!(prepared
        .pairs
        .iter()
        .filter(|pair| pair.identity.kind == ComponentKind::Agent)
        .all(|pair| pair.identity == tidy));
}

#[test]
fn prepared_generated_reference_rewrites_to_published_alias_and_keeps_opaque_text() {
    let demo = identity("demo", "demo", ComponentKind::Skill, "demo-pipeline");
    let helper = identity("provider", "plugin", ComponentKind::Agent, "helper");
    let inventory = DeliveryInventory {
        components: vec![component(helper.clone()), component(demo.clone())],
        foreign_components: vec![],
    };
    let persisted = vec![TargetAlias::new(Target::Claude, demo, "demo-pipeline-v2").unwrap()];
    let mut bodies = ComponentBodies::new();
    bodies.insert(
        helper.clone(),
        Ok("Run /demo-pipeline, then /help. Raw demo-pipeline text stays.\n".into()),
    );

    let prepared = prepare_delivery(
        &inventory,
        &[Target::Claude],
        &persisted,
        &UnconvertibleCommands::new(),
        &bodies,
    );

    assert_eq!(prepared.payloads.len(), 1);
    assert_eq!(
        prepared.payloads[0].body,
        "Run /demo-pipeline-v2, then /help. Raw demo-pipeline text stays.\n"
    );
}

#[test]
fn final_reference_rewrite_uses_fallback_routes() {
    let demo = identity("demo", "demo", ComponentKind::Skill, "demo-pipeline");
    let agent = identity("provider", "plugin", ComponentKind::Agent, "runner");
    let inventory = DeliveryInventory {
        components: vec![component(agent.clone()), component(demo.clone())],
        foreign_components: vec![],
    };
    let persisted = vec![TargetAlias::new(Target::Claude, demo, "demo-pipeline-v2").unwrap()];
    let mut bodies = ComponentBodies::new();
    bodies.insert(agent.clone(), Ok("Run /demo-pipeline now.\n".into()));
    let adapters = NativeAdapters::from([(
        Target::Claude,
        NativeAdapterState::Failed {
            reason: "fixture failure".into(),
        },
    )]);

    let prepared = prepare_delivery_with_native_routes(
        &inventory,
        &[Target::Claude],
        &persisted,
        &UnconvertibleCommands::new(),
        &bodies,
        &adapters,
        |_| {},
    );

    assert!(matches!(
        prepared_route(&prepared.plan, &agent, Target::Claude).native,
        NativeRouteStatus::Fallback { .. }
    ));
    assert_eq!(prepared.payloads.len(), 1);
    assert_eq!(prepared.payloads[0].body, "Run /demo-pipeline-v2 now.\n");
}

#[test]
fn final_reference_rewrite_command_collision_blocks_the_group() {
    let colliding_skill = identity("provider", "plugin", ComponentKind::Skill, "build");
    let command = identity("provider", "plugin", ComponentKind::Command, "build");
    let sibling = identity("provider", "plugin", ComponentKind::Command, "clean");
    let inventory = DeliveryInventory {
        components: vec![
            component(colliding_skill),
            component(command.clone()),
            component(sibling.clone()),
        ],
        foreign_components: vec![],
    };
    let mut bodies = ComponentBodies::new();
    bodies.insert(command.clone(), Ok("Run /build before cleanup.\n".into()));
    bodies.insert(sibling.clone(), Ok("Clean command body.\n".into()));
    let adapters = NativeAdapters::from([(Target::Codex, NativeAdapterState::Ready)]);

    let prepared = prepare_delivery_with_native_routes(
        &inventory,
        &[Target::Codex],
        &[],
        &UnconvertibleCommands::new(),
        &bodies,
        &adapters,
        |_| {},
    );

    for identity in [&command, &sibling] {
        assert!(matches!(
            prepared_route(&prepared.plan, identity, Target::Codex).choice,
            ComponentRouteChoice::Blocked { .. }
        ));
        assert!(!prepared
            .payloads
            .iter()
            .any(|payload| payload.identity == *identity));
        assert!(matches!(
            prepared_route(&prepared.plan, identity, Target::Codex).native,
            NativeRouteStatus::Fallback { .. }
        ));
    }
}

#[test]
fn final_reference_rewrite_uses_loose_routes_after_rejected_physical_identity() {
    let demo = identity("demo", "demo", ComponentKind::Skill, "demo-pipeline");
    let agent = identity("provider", "plugin", ComponentKind::Agent, "runner");
    let inventory = DeliveryInventory {
        components: vec![component(agent.clone()), component(demo.clone())],
        foreign_components: vec![],
    };
    let persisted = vec![TargetAlias::new(Target::Claude, demo, "demo-pipeline-v2").unwrap()];
    let mut bodies = ComponentBodies::new();
    bodies.insert(agent.clone(), Ok("Run /demo-pipeline now.\n".into()));
    let adapters = NativeAdapters::from([(Target::Claude, NativeAdapterState::Ready)]);

    let prepared = prepare_delivery_with_native_routes(
        &inventory,
        &[Target::Claude],
        &persisted,
        &UnconvertibleCommands::new(),
        &bodies,
        &adapters,
        |plan| {
            prepared_route_mut(plan, &agent, Target::Claude).physical_identity =
                PhysicalIdentity::Rejected;
        },
    );

    let agent_route = prepared_route(&prepared.plan, &agent, Target::Claude);
    assert!(matches!(
        agent_route.native,
        NativeRouteStatus::Fallback { .. }
    ));
    assert!(matches!(
        agent_route.choice,
        ComponentRouteChoice::Direct { .. } | ComponentRouteChoice::ServedByRoot { .. }
    ));
    assert_eq!(prepared.payloads.len(), 1);
    assert_eq!(prepared.payloads[0].identity, agent);
    assert_eq!(prepared.payloads[0].body, "Run /demo-pipeline-v2 now.\n");
}

#[test]
fn prepared_agent_without_source_content_is_pending_and_blocked() {
    let late = identity("provider", "plugin", ComponentKind::Agent, "late");
    let prepared = prepare_delivery(
        &DeliveryInventory {
            components: vec![component(late.clone())],
            foreign_components: vec![],
        },
        &[Target::Claude],
        &[],
        &UnconvertibleCommands::new(),
        &ComponentBodies::new(),
    );

    assert_eq!(
        prepared.pending,
        vec![PendingReference {
            identity: late.clone(),
            target: Target::Claude,
        }]
    );
    assert!(matches!(
        prepared_route(&prepared.plan, &late, Target::Claude).choice,
        ComponentRouteChoice::Blocked { ref reason } if reason.contains("pending")
    ));
    assert!(prepared.payloads.is_empty());
    assert!(prepared.pairs.is_empty());
}

#[test]
fn tp08_spec_block_shared_root_consumer_and_observe_pairs_or_alternate_roots() {
    // Shared-root veto: block one shared-root consumer/component and observe only approved
    // pairs or permitted alternate roots, with affected consumers reported.
    //
    // Claude and OpenCode share ~/.claude/skills. When a conflict or blocked
    // disposition blocks Claude for a component, Claude vetoes writing that
    // component into the shared root. OpenCode falls back to its safe alternate
    // root ~/.config/opencode/skills, and the withheld pair reports affected consumers.
    let review = identity("provider", "shared-plugin", ComponentKind::Skill, "review");
    let lint = identity("other", "shared-plugin", ComponentKind::Skill, "lint");
    let inventory = DeliveryInventory {
        components: vec![component(review.clone()), component(lint.clone())],
        foreign_components: vec![],
    };

    let prepared = prepare_delivery(
        &inventory,
        &[Target::Claude, Target::Opencode],
        &[],
        &UnconvertibleCommands::new(),
        &ComponentBodies::new(),
    );

    // Claude is blocked for review because of the shared-plugin multi-source conflict.
    let claude_route = prepared_route(&prepared.plan, &review, Target::Claude);
    assert!(
        matches!(claude_route.choice, ComponentRouteChoice::Blocked { .. }),
        "expected Claude review route to be Blocked, got: {:?}",
        claude_route.choice
    );

    // OpenCode has a permitted alternate root (~/.config/opencode/skills)
    let opencode_route = prepared_route(&prepared.plan, &review, Target::Opencode);
    assert_eq!(
        opencode_route.choice,
        ComponentRouteChoice::Direct {
            root: "~/.config/opencode/skills".into()
        }
    );

    // Only approved component-root pairs are present.
    // The forbidden pair (~/.claude/skills, review) MUST NOT be present.
    assert!(
        prepared
            .pairs
            .iter()
            .all(|pair| !(pair.identity == review && pair.root == "~/.claude/skills")),
        "forbidden shared root pair found in prepared pairs: {:?}",
        prepared.pairs
    );
    assert!(prepared.pairs.contains(&ComponentRootPair {
        identity: review.clone(),
        root: "~/.config/opencode/skills".into(),
    }));

    // The shared root table must not contain review
    assert!(
        prepared
            .plan
            .shared_roots
            .iter()
            .all(|sr| !sr.components.contains(&review)),
        "vetoed component must not be in shared_roots: {:?}",
        prepared.plan.shared_roots
    );

    // Withheld pairs record the vetoed root and affected consumers
    let withheld = prepared
        .withheld
        .iter()
        .find(|w| w.pair.identity == review)
        .expect("review must be in withheld pairs");
    assert_eq!(withheld.pair.root, "~/.claude/skills");
    assert_eq!(withheld.blocked_consumers, vec![Target::Claude]);
    assert_eq!(withheld.consumers, vec![Target::Claude, Target::Opencode]);
}

#[test]
fn tp09_spec_prepare_alias_map_observe_rewritten_spans_opaque_prose_and_blocked_ambiguous_unit() {
    // Alias map: prepare one provider/root alias map and observe changes only in
    // the intended generated invocation spans, unchanged published opaque
    // prose/resource bytes, and unchanged immutable generations. An ambiguous
    // unit publishes no member.
    let demo = identity("demo", "demo", ComponentKind::Skill, "demo-pipeline");
    let runner = identity("my-tool", "my-tool", ComponentKind::Agent, "runner");
    let sidecar = identity("my-tool", "my-tool", ComponentKind::Agent, "sidecar");
    let other_agent = identity("independent", "independent", ComponentKind::Agent, "helper");

    let inventory = DeliveryInventory {
        components: vec![
            component(runner.clone()),
            component(sidecar.clone()),
            component(other_agent.clone()),
            component(demo.clone()),
        ],
        foreign_components: vec![],
    };

    // 1. Test alias rewriting preserves opaque prose
    let persisted = vec![TargetAlias::new(Target::Claude, demo.clone(), "demo-pipeline-v2").unwrap()];
    let mut bodies = ComponentBodies::new();
    let runner_original =
        "Run /demo-pipeline now.\nCode: `fn do_work() {}`\nRaw demo-pipeline text stays unaliased.\n";
    bodies.insert(runner.clone(), Ok(runner_original.into()));
    bodies.insert(sidecar.clone(), Ok("Sidecar clean text.\n".into()));
    bodies.insert(
        other_agent.clone(),
        Ok("Helper agent with no commands.\n".into()),
    );

    let prepared = prepare_delivery(
        &inventory,
        &[Target::Claude],
        &persisted,
        &UnconvertibleCommands::new(),
        &bodies,
    );

    // Runner's invocation /demo-pipeline should rewrite to /demo-pipeline-v2, while code and raw text stay byte-identical
    let runner_payload = prepared
        .payloads
        .iter()
        .find(|p| p.identity == runner && p.target == Target::Claude)
        .expect("runner payload must be present");
    assert_eq!(
        runner_payload.body,
        "Run /demo-pipeline-v2 now.\nCode: `fn do_work() {}`\nRaw demo-pipeline text stays unaliased.\n"
    );

    // 2. Test pending validation when content is unavailable
    let pending_agent = identity("provider", "plugin", ComponentKind::Agent, "pending-one");
    let pending_prep = prepare_delivery(
        &DeliveryInventory {
            components: vec![component(pending_agent.clone())],
            foreign_components: vec![],
        },
        &[Target::Claude],
        &[],
        &UnconvertibleCommands::new(),
        &ComponentBodies::new(),
    );
    assert_eq!(
        pending_prep.pending,
        vec![PendingReference {
            identity: pending_agent.clone(),
            target: Target::Claude,
        }]
    );
    assert!(matches!(
        prepared_route(&pending_prep.plan, &pending_agent, Target::Claude).choice,
        ComponentRouteChoice::Blocked { ref reason } if reason.contains("pending")
    ));
    assert!(pending_prep.payloads.is_empty());
    assert!(pending_prep.pairs.is_empty());

    // 3. Test ambiguous reference blocks its whole unit
    let ambiguous_inv = DeliveryInventory {
        components: vec![
            component(runner.clone()),
            component(sidecar.clone()),
            component(other_agent.clone()),
            component(identity("a", "plugin-a", ComponentKind::Skill, "ambig")),
            component(identity("b", "plugin-b", ComponentKind::Skill, "ambig")),
        ],
        foreign_components: vec![],
    };

    let mut ambig_bodies = ComponentBodies::new();
    // runner references ambiguous /ambig
    ambig_bodies.insert(
        runner.clone(),
        Ok("Please call /ambig to proceed.\n".into()),
    );
    // sidecar has clean text without ambiguous references, but belongs to the same provider/unit
    ambig_bodies.insert(sidecar.clone(), Ok("I am clean prose.\n".into()));
    // other_agent belongs to independent provider
    ambig_bodies.insert(other_agent.clone(), Ok("Independent clean prose.\n".into()));

    let ambig_prepared = prepare_delivery(
        &ambiguous_inv,
        &[Target::Claude],
        &[],
        &UnconvertibleCommands::new(),
        &ambig_bodies,
    );

    // Both runner and sidecar must be blocked because the unit is blocked
    for blocked_id in [&runner, &sidecar] {
        let route = prepared_route(&ambig_prepared.plan, blocked_id, Target::Claude);
        assert!(
            matches!(route.choice, ComponentRouteChoice::Blocked { ref reason } if reason.contains("cannot be rewritten")),
            "expected {:?} to be blocked, got {:?}",
            blocked_id,
            route.choice
        );
        assert!(
            ambig_prepared
                .payloads
                .iter()
                .all(|p| p.identity != *blocked_id),
            "blocked member {:?} must not be in payloads",
            blocked_id
        );
        assert!(
            ambig_prepared
                .pairs
                .iter()
                .all(|p| p.identity != *blocked_id),
            "blocked member {:?} must not be in pairs",
            blocked_id
        );
    }

    // Independent agent from another provider is NOT blocked
    let independent_route = prepared_route(&ambig_prepared.plan, &other_agent, Target::Claude);
    assert!(matches!(
        independent_route.choice,
        ComponentRouteChoice::Direct { .. }
    ));
    assert!(ambig_prepared
        .payloads
        .iter()
        .any(|p| p.identity == other_agent));
    assert!(ambig_prepared
        .pairs
        .iter()
        .any(|p| p.identity == other_agent));
}

fn native_status<'a>(
    plan: &'a DeliveryPlan,
    identity: &ComponentIdentity,
    target: Target,
) -> &'a NativeRouteStatus {
    &plan
        .routes
        .iter()
        .find(|route| route.identity == *identity && route.target == target)
        .unwrap()
        .native
}

fn shared_root_consumers(plan: &DeliveryPlan, root: &str) -> Vec<Target> {
    plan.shared_roots
        .iter()
        .find(|contents| contents.root == root)
        .unwrap()
        .reached_consumers
        .clone()
}

#[test]
fn p7_t09_mixed_selection_keeps_one_route_per_enabled_component_and_groups_by_plugin() {
    let reviewer = identity("provider", "alpha", ComponentKind::Agent, "reviewer");
    let review = identity("provider", "beta", ComponentKind::Skill, "review");
    let retired = identity("provider", "alpha", ComponentKind::Agent, "retired");
    let inventory = DeliveryInventory {
        components: vec![
            component(reviewer.clone()),
            component(review.clone()),
            DeliveryComponent {
                enabled: false,
                ..component(retired.clone())
            },
        ],
        foreign_components: Vec::new(),
    };
    let mut plan = plan_delivery(&inventory, &[Target::Claude], &[]);
    assert_eq!(plan.routes.len(), 3);
    let adapters = NativeAdapters::from([(Target::Claude, NativeAdapterState::Ready)]);
    plan_native_routes(&mut plan, &adapters);

    assert_eq!(plan.routes.len(), 3);
    assert_eq!(
        native_status(&plan, &reviewer, Target::Claude),
        &NativeRouteStatus::Native,
        "a private agent root has no shared copy, so its group activates"
    );
    assert!(matches!(
        native_status(&plan, &review, Target::Claude),
        NativeRouteStatus::Fallback { .. }
    ));
    let disabled = plan
        .routes
        .iter()
        .find(|route| route.identity == retired)
        .unwrap();
    assert_eq!(disabled.choice, ComponentRouteChoice::Disabled);
    assert_eq!(disabled.native, NativeRouteStatus::Loose);
}

#[test]
fn p7_t09_retained_shared_copy_blocks_native_and_keeps_unselected_reader() {
    let review = identity("provider", "beta", ComponentKind::Skill, "review");
    let inventory = DeliveryInventory {
        components: vec![component(review.clone())],
        foreign_components: Vec::new(),
    };
    let mut plan = plan_delivery(&inventory, &[Target::Claude], &[]);
    let adapters = NativeAdapters::from([(Target::Claude, NativeAdapterState::Ready)]);
    plan_native_routes(&mut plan, &adapters);

    assert_eq!(
        shared_root_consumers(&plan, "~/.claude/skills"),
        vec![Target::Claude, Target::Opencode],
        "the unselected reader stays in the shared root's consumers"
    );
    let status = native_status(&plan, &review, Target::Claude).clone();
    let NativeRouteStatus::Fallback { reason } = &status else {
        panic!("expected fallback, got {status:?}");
    };
    assert!(reason.contains("~/.claude/skills"), "{reason}");
    assert_eq!(
        route(&plan, Target::Claude).choice,
        ComponentRouteChoice::ServedByRoot {
            root: "~/.claude/skills".into()
        },
        "the loose route stays active"
    );
}

#[test]
fn p7_t09_native_registration_failure_keeps_loose_route_as_fallback() {
    let reviewer = identity("provider", "alpha", ComponentKind::Agent, "reviewer");
    let inventory = DeliveryInventory {
        components: vec![component(reviewer.clone())],
        foreign_components: Vec::new(),
    };

    let mut ready = plan_delivery(&inventory, &[Target::Codex], &[]);
    let ready_adapters = NativeAdapters::from([(Target::Codex, NativeAdapterState::Ready)]);
    plan_native_routes(&mut ready, &ready_adapters);
    assert_eq!(
        native_status(&ready, &reviewer, Target::Codex),
        &NativeRouteStatus::Native
    );

    let mut failed = plan_delivery(&inventory, &[Target::Codex], &[]);
    let failed_adapters = NativeAdapters::from([(
        Target::Codex,
        NativeAdapterState::Failed {
            reason: "registration refused".into(),
        },
    )]);
    plan_native_routes(&mut failed, &failed_adapters);
    let failed_route = route(&failed, Target::Codex);
    assert_eq!(
        failed_route.choice,
        ComponentRouteChoice::Direct {
            root: "~/.codex/agents".into()
        },
        "the loose route stays active"
    );
    let NativeRouteStatus::Fallback { reason } = &failed_route.native else {
        panic!("expected fallback, got {:?}", failed_route.native);
    };
    assert!(reason.contains("registration refused"), "{reason}");
    let note = native_status_note(failed_route).expect("fallback has a note");
    assert!(note.contains("loose route active"), "{note}");
    assert!(note.contains("registration refused"), "{note}");
}

#[test]
fn p7_t09_observed_targets_and_opencode_stay_loose() {
    let review = identity("provider", "beta", ComponentKind::Skill, "review");
    let inventory = DeliveryInventory {
        components: vec![component(review.clone())],
        foreign_components: Vec::new(),
    };
    let targets = [Target::Copilot, Target::AgyIde, Target::Opencode];
    let mut plan = plan_delivery(&inventory, &targets, &[]);
    let adapters = NativeAdapters::from([
        (Target::Copilot, NativeAdapterState::Observed),
        (Target::AgyIde, NativeAdapterState::Observed),
        (Target::Opencode, NativeAdapterState::Ready),
    ]);
    plan_native_routes(&mut plan, &adapters);

    for target in [Target::Copilot, Target::AgyIde] {
        let observed = plan
            .routes
            .iter()
            .find(|route| route.target == target)
            .unwrap();
        assert!(
            matches!(
                observed.choice,
                ComponentRouteChoice::Direct { .. } | ComponentRouteChoice::ServedByRoot { .. }
            ),
            "{target:?} keeps its loose route"
        );
        assert_eq!(observed.native, NativeRouteStatus::ObservedLoose);
    }
    assert_eq!(
        native_status(&plan, &review, Target::Opencode),
        &NativeRouteStatus::Loose,
        "OpenCode is not native-capable, so Ready does not activate it"
    );
}

#[test]
fn p7_t09_observed_copilot_keeps_its_private_copy_and_unselected_opencode_blocks_codex() {
    let review = identity("provider", "beta", ComponentKind::Skill, "review");
    let inventory = DeliveryInventory {
        components: vec![component(review.clone())],
        foreign_components: Vec::new(),
    };
    let mut plan = plan_delivery(&inventory, &[Target::Codex, Target::Copilot], &[]);
    let adapters = NativeAdapters::from([
        (Target::Codex, NativeAdapterState::Ready),
        (Target::Copilot, NativeAdapterState::Observed),
    ]);
    plan_native_routes(&mut plan, &adapters);

    let status = native_status(&plan, &review, Target::Codex).clone();
    let NativeRouteStatus::Fallback { reason } = &status else {
        panic!("expected fallback, got {status:?}");
    };
    assert!(reason.contains("~/.agents/skills"), "{reason}");
    assert!(
        reason.contains("opencode") && !reason.contains("copilot"),
        "Copilot keeps its private copy, so only the unselected OpenCode retains the shared one: {reason}"
    );
    assert_eq!(
        native_status(&plan, &review, Target::Copilot),
        &NativeRouteStatus::ObservedLoose
    );
}

#[test]
fn p7_t09_default_adapters_keep_every_route_loose() {
    let review = identity("provider", "beta", ComponentKind::Skill, "review");
    let reviewer = identity("provider", "alpha", ComponentKind::Agent, "reviewer");
    let inventory = DeliveryInventory {
        components: vec![component(review), component(reviewer)],
        foreign_components: Vec::new(),
    };
    let baseline = plan_delivery(&inventory, &[Target::Claude, Target::Codex], &[]);
    let mut plan = baseline.clone();
    plan_native_routes(&mut plan, &NativeAdapters::new());

    assert!(plan
        .routes
        .iter()
        .all(|route| route.native == NativeRouteStatus::Loose));
    let choices = |plan: &DeliveryPlan| {
        plan.routes
            .iter()
            .map(|route| route.choice.clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(choices(&plan), choices(&baseline));
    assert_eq!(native_status_note(&plan.routes[0]), None);
}

#[test]
fn final_group_routes_emit_copilot_and_agy_faces_and_skip_disabled_components() {
    use ccync_foundation::target::CANONICAL_CLIENT_FACES;

    let enabled = identity("provider", "beta", ComponentKind::Skill, "faces");
    let disabled = identity("provider", "beta", ComponentKind::Agent, "disabled");
    let inventory = DeliveryInventory {
        components: vec![
            component(enabled.clone()),
            DeliveryComponent {
                identity: disabled,
                provider_hooks_declared: false,
                enabled: false,
                references_preserved: true,
            },
        ],
        foreign_components: Vec::new(),
    };
    let plan = plan_delivery(
        &inventory,
        &[
            Target::Claude,
            Target::Codex,
            Target::Copilot,
            Target::Opencode,
            Target::AgyCli,
            Target::AgyIde,
            Target::AgyGui,
        ],
        &[],
    );
    let faces = plan_final_group_routes(&plan, &NativeAdapters::new());
    let enabled_faces = faces
        .iter()
        .filter(|route| route.identity == enabled)
        .map(|route| route.face)
        .collect::<Vec<_>>();

    assert_eq!(enabled_faces, CANONICAL_CLIENT_FACES);
    assert_eq!(
        faces
            .iter()
            .filter(|route| route.identity == enabled && route.face == ClientFace::CopilotCli)
            .count(),
        1
    );
    assert_eq!(
        faces
            .iter()
            .filter(|route| route.identity == enabled && route.face == ClientFace::CopilotVsCode)
            .count(),
        1
    );
    assert!(faces
        .iter()
        .any(|route| route.identity == enabled && route.face == ClientFace::AgyCli));
    assert!(faces
        .iter()
        .any(|route| route.identity == enabled && route.face == ClientFace::AgyIde));
    assert!(faces
        .iter()
        .any(|route| route.identity == enabled && route.face == ClientFace::AgyGui));
    assert!(faces
        .iter()
        .all(|route| route.identity
            != identity("provider", "beta", ComponentKind::Agent, "disabled")));
}

#[test]
fn final_group_routes_block_required_unsupported_member() {
    let skill = identity("provider", "beta", ComponentKind::Skill, "review");
    let resource = identity("provider", "beta", ComponentKind::Resource, "metadata");
    let plan = plan_delivery(
        &DeliveryInventory {
            components: vec![component(skill.clone()), component(resource)],
            foreign_components: Vec::new(),
        },
        &[Target::Claude],
        &[],
    );
    let adapters = NativeAdapters::from([(Target::Claude, NativeAdapterState::Ready)]);
    let faces = plan_final_group_routes(&plan, &adapters);

    assert!(matches!(
        faces
            .iter()
            .find(|route| route.identity == skill)
            .unwrap()
            .native,
        NativeRouteStatus::Fallback { .. }
    ));
}

#[test]
fn final_group_routes_block_unresolved_alias() {
    let review = identity("provider", "beta", ComponentKind::Skill, "review");
    let mut plan = plan_delivery(
        &DeliveryInventory {
            components: vec![component(review.clone())],
            foreign_components: Vec::new(),
        },
        &[Target::Claude],
        &[],
    );
    plan.routes[0].foreign_alias_path = Some("~/.claude/skills/review".into());
    plan.routes[0].alias = None;
    let adapters = NativeAdapters::from([(Target::Claude, NativeAdapterState::Ready)]);
    let faces = plan_final_group_routes(&plan, &adapters);

    assert!(matches!(
        &faces[0].native,
        NativeRouteStatus::Fallback { reason } if reason.contains("no resolved alias")
    ));
}

#[test]
fn final_group_routes_block_escaping_physical_identity() {
    let review = identity("provider", "beta", ComponentKind::Skill, "review");
    let mut plan = plan_delivery(
        &DeliveryInventory {
            components: vec![component(review.clone())],
            foreign_components: Vec::new(),
        },
        &[Target::Claude],
        &[],
    );
    plan.routes[0].physical_identity =
        PhysicalIdentity::Validated("C:\\Users\\test\\..\\outside".into());
    let adapters = NativeAdapters::from([(Target::Claude, NativeAdapterState::Ready)]);
    let face_routes = plan_final_group_routes(&plan, &adapters);

    assert_eq!(face_routes.len(), 1);
    assert!(matches!(
        &face_routes[0].native,
        NativeRouteStatus::Fallback { reason } if reason.contains("escaping")
    ));
}

#[test]
fn final_group_routes_block_rejected_physical_identity() {
    let review = identity("provider", "beta", ComponentKind::Skill, "review");
    let mut plan = plan_delivery(
        &DeliveryInventory {
            components: vec![component(review)],
            foreign_components: Vec::new(),
        },
        &[Target::Claude],
        &[],
    );
    plan.routes[0].physical_identity = PhysicalIdentity::Rejected;
    let adapters = NativeAdapters::from([(Target::Claude, NativeAdapterState::Ready)]);
    let face_routes = plan_final_group_routes(&plan, &adapters);

    assert_eq!(face_routes.len(), 1);
    assert!(matches!(
        &face_routes[0].native,
        NativeRouteStatus::Fallback { reason } if reason.contains("escaping or unresolved")
    ));
}

#[test]
fn final_group_routes_keep_root_string_comparison_for_unchecked_identity() {
    let first = identity("provider", "beta", ComponentKind::Skill, "review-a");
    let second = identity("provider", "beta", ComponentKind::Skill, "review-b");
    let mut plan = plan_delivery(
        &DeliveryInventory {
            components: vec![component(first.clone()), component(second)],
            foreign_components: Vec::new(),
        },
        &[Target::Claude],
        &[],
    );
    for route in &mut plan.routes {
        route.alias = Some("same-name".into());
    }
    plan.routes[0].physical_identity = PhysicalIdentity::Unchecked;
    plan.routes[1].physical_identity = PhysicalIdentity::Validated("C:\\somewhere\\else".into());
    let adapters = NativeAdapters::from([(Target::Claude, NativeAdapterState::Ready)]);
    let face_routes = plan_final_group_routes(&plan, &adapters);

    assert_eq!(face_routes.len(), 2);
    assert!(face_routes.iter().all(|route| matches!(
        &route.native,
        NativeRouteStatus::Fallback { reason } if reason.contains("namespace collision")
    )));
    assert_eq!(
        face_routes
            .iter()
            .find(|route| route.identity == first)
            .unwrap()
            .physical_identity,
        PhysicalIdentity::Unchecked
    );
}

// Case-insensitive physical roots and `C:\` absolute paths exist only on Windows.
#[cfg(windows)]
#[test]
fn final_group_routes_block_case_aliased_namespace_collision() {
    let first = identity("provider", "alpha", ComponentKind::Skill, "review");
    let second = identity("provider", "beta", ComponentKind::Skill, "review");
    let mut plan = plan_delivery(
        &DeliveryInventory {
            components: vec![component(first.clone()), component(second.clone())],
            foreign_components: Vec::new(),
        },
        &[Target::Claude],
        &[],
    );
    for route in &mut plan.routes {
        route.alias = Some("same-name".into());
        route.physical_identity = PhysicalIdentity::Validated(if route.identity == first {
            "C:\\Users\\test\\.claude\\skills".into()
        } else {
            "c:\\users\\TEST\\.CLAUDE\\SKILLS".into()
        });
    }
    let adapters = NativeAdapters::from([(Target::Claude, NativeAdapterState::Ready)]);
    let face_routes = plan_final_group_routes(&plan, &adapters);

    assert_eq!(face_routes.len(), 2);
    assert!(face_routes.iter().all(|route| matches!(
        &route.native,
        NativeRouteStatus::Fallback { reason } if reason.contains("namespace collision")
    )));
}

// ── shared-root reader coverage ─────────────────────────────────────────────
//
// A reader of a shared root does not retain the shared copy when it still
// finds the component elsewhere after the copy retires: through its own
// native route, through another shared root whose copy stays, or through a
// copy in its private root.

fn skill_plan(selected: &[Target], adapters: &NativeAdapters) -> (DeliveryPlan, ComponentIdentity) {
    let review = identity("provider", "beta", ComponentKind::Skill, "review");
    let inventory = DeliveryInventory {
        components: vec![component(review.clone())],
        foreign_components: Vec::new(),
    };
    let mut plan = plan_delivery(&inventory, selected, &[]);
    plan_native_routes(&mut plan, adapters);
    (plan, review)
}

fn fallback_reason(plan: &DeliveryPlan, identity: &ComponentIdentity, target: Target) -> String {
    match native_status(plan, identity, target) {
        NativeRouteStatus::Fallback { reason } => reason.clone(),
        other => panic!("expected fallback on {target}, got {other:?}"),
    }
}

#[test]
fn shared_reader_coverage_copilot_private_copy_lets_codex_activate() {
    let adapters = NativeAdapters::from([(Target::Codex, NativeAdapterState::Ready)]);
    let (plan, review) = skill_plan(
        &[Target::Codex, Target::Copilot, Target::Opencode],
        &adapters,
    );

    assert_eq!(
        route(&plan, Target::Copilot).choice,
        ComponentRouteChoice::Direct {
            root: "~/.copilot/skills".into()
        },
        "Copilot has its own copy"
    );
    assert_eq!(
        native_status(&plan, &review, Target::Codex),
        &NativeRouteStatus::Native,
        "Copilot and OpenCode keep the skill without the shared copy"
    );
}

#[test]
fn shared_reader_coverage_kept_agents_root_covers_opencode_for_claude() {
    let adapters = NativeAdapters::from([(Target::Claude, NativeAdapterState::Ready)]);
    let (plan, review) = skill_plan(
        &[Target::Claude, Target::Codex, Target::Opencode],
        &adapters,
    );

    assert_eq!(
        native_status(&plan, &review, Target::Claude),
        &NativeRouteStatus::Native,
        "OpenCode still finds the skill in ~/.agents/skills, which the loose Codex route keeps"
    );
    assert_eq!(
        route(&plan, Target::Opencode).choice,
        ComponentRouteChoice::ServedByRoot {
            root: "~/.agents/skills".into()
        }
    );
}

#[test]
fn shared_reader_coverage_unselected_opencode_is_covered_by_a_kept_shared_root() {
    let adapters = NativeAdapters::from([(Target::Claude, NativeAdapterState::Ready)]);
    let (plan, review) = skill_plan(&[Target::Claude, Target::Codex], &adapters);

    assert_eq!(
        shared_root_consumers(&plan, "~/.claude/skills"),
        vec![Target::Claude, Target::Opencode],
        "the unselected reader stays in the shared root's consumers"
    );
    assert_eq!(
        native_status(&plan, &review, Target::Claude),
        &NativeRouteStatus::Native,
        "the unselected OpenCode keeps the identical copy in ~/.agents/skills"
    );
}

#[test]
fn shared_reader_coverage_both_shared_roots_retiring_leave_opencode_its_private_root() {
    let adapters = NativeAdapters::from([
        (Target::Claude, NativeAdapterState::Ready),
        (Target::Codex, NativeAdapterState::Ready),
    ]);
    let (plan, review) = skill_plan(
        &[
            Target::Claude,
            Target::Codex,
            Target::Copilot,
            Target::Opencode,
        ],
        &adapters,
    );

    assert_eq!(
        native_status(&plan, &review, Target::Claude),
        &NativeRouteStatus::Native
    );
    assert_eq!(
        native_status(&plan, &review, Target::Codex),
        &NativeRouteStatus::Native
    );
}

#[test]
fn shared_reader_coverage_unselected_copilot_still_blocks_codex() {
    let adapters = NativeAdapters::from([(Target::Codex, NativeAdapterState::Ready)]);
    let (plan, review) = skill_plan(&[Target::Codex, Target::Opencode], &adapters);

    let reason = fallback_reason(&plan, &review, Target::Codex);
    assert!(reason.contains("~/.agents/skills"), "{reason}");
    assert!(
        reason.contains("copilot") && !reason.contains("opencode"),
        "only the unselected Copilot has no other copy: {reason}"
    );
}

#[test]
fn shared_reader_coverage_unselected_opencode_keeps_one_shared_copy_and_one_group_native() {
    let adapters = NativeAdapters::from([
        (Target::Claude, NativeAdapterState::Ready),
        (Target::Codex, NativeAdapterState::Ready),
    ]);
    let (plan, review) = skill_plan(&[Target::Claude, Target::Codex, Target::Copilot], &adapters);

    let reason = fallback_reason(&plan, &review, Target::Claude);
    assert!(
        reason.contains("~/.claude/skills") && reason.contains("opencode"),
        "both shared copies would retire and the unselected OpenCode has no private copy: {reason}"
    );
    assert_eq!(
        native_status(&plan, &review, Target::Codex),
        &NativeRouteStatus::Native,
        "with the Claude group kept loose, OpenCode keeps the copy in ~/.claude/skills"
    );
}

#[test]
fn shared_reader_coverage_every_target_selected_activates_claude_and_codex() {
    let adapters = NativeAdapters::from([
        (Target::Claude, NativeAdapterState::Ready),
        (Target::Codex, NativeAdapterState::Ready),
    ]);
    let all = [
        Target::Claude,
        Target::Codex,
        Target::Copilot,
        Target::Opencode,
        Target::AgyCli,
        Target::AgyIde,
        Target::AgyGui,
    ];
    let (plan, review) = skill_plan(&all, &adapters);

    assert_eq!(
        native_status(&plan, &review, Target::Claude),
        &NativeRouteStatus::Native
    );
    assert_eq!(
        native_status(&plan, &review, Target::Codex),
        &NativeRouteStatus::Native
    );
}

#[test]
fn shared_reader_coverage_command_skill_in_a_shared_root_still_blocks_codex() {
    let audit = identity("provider", "beta", ComponentKind::Command, "audit");
    let inventory = DeliveryInventory {
        components: vec![component(audit.clone())],
        foreign_components: Vec::new(),
    };
    let mut plan = plan_delivery(
        &inventory,
        &[Target::Codex, Target::Copilot, Target::Opencode],
        &[],
    );
    let adapters = NativeAdapters::from([(Target::Codex, NativeAdapterState::Ready)]);
    plan_native_routes(&mut plan, &adapters);

    let reason = fallback_reason(&plan, &audit, Target::Codex);
    assert!(
        reason.contains("~/.agents/skills"),
        "only a skill copy can cover a reader, so a command-skill keeps its shared copy: {reason}"
    );
}
