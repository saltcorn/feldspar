// The Plan & build screen's progress and phases, rendered from a construction
// (TODO-app-constructor AC4.4).

import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";

import type { Construction } from "../construction";
import {
  Construct,
  PhaseDetail,
  PhasesCard,
  ProgressCard,
  MockupsCard,
  QuestionsCard,
  SpecificationCard,
  StepTabs,
} from "./Construct";

function construction(status: string, taskStatus: string): Construction {
  return {
    id: "c1",
    application: "a1",
    status,
    agent: "construct-shop",
    specification: "A shop.",
    started_by: null,
    plan_run: null,
    planning: false,
    error: status === "failed" ? "task `Books list` failed: the build is red" : null,
    until_phase: null,
    plan_only: false,
    designing: false,
    asking: false,
    questions: [],
    mockups: [],
    updated_at: null,
    phases: [
      {
        id: "p1",
        title: "Data model",
        description: "",
        requirements: [{ text: "books and authors", priority: "must" }],
        status: taskStatus === "failed" ? "failed" : "running",
        tasks: [
          {
            id: "t1",
            kind: "feature",
            title: "Books list",
            brief: "",
            depends_on: [],
            status: taskStatus,
            runs: ["r1", "r2"],
            summary: "the build is red",
            created: [],
          },
        ],
      },
      {
        id: "p2",
        title: "Checkout",
        description: "",
        requirements: [],
        status: "unplanned",
        tasks: [],
      },
    ],
  };
}

const noop = () => {};

describe("Construct", () => {
  it("renders before anything has loaded", () => {
    const html = renderToStaticMarkup(<Construct appId="a1" />);
    expect(html).toContain("Plan &amp; build");
  });
});

describe("the progress", () => {
  it("shows a failed task with Retry and Skip, its runs, and the error", () => {
    const html = renderToStaticMarkup(
      <ProgressCard
        construction={construction("failed", "failed")}
        busy={false}
        onRetry={noop}
        onSkip={noop}
      />,
    );
    expect(html).toContain("Retry");
    expect(html).toContain("Skip");
    expect(html).toContain('href="#/runs/r1"');
    expect(html).toContain('href="#/runs/r2"');
    expect(html).toContain("task `Books list` failed");
    expect(html).toContain("0 of 2 phases done");
    expect(html).toContain("Its tasks are planned when the phase is reached.");
  });

  it("offers neither while the construction runs", () => {
    const html = renderToStaticMarkup(
      <ProgressCard
        construction={construction("running", "running")}
        busy={false}
        onRetry={noop}
        onSkip={noop}
      />,
    );
    expect(html).not.toContain("Retry");
    expect(html).not.toContain(">Skip<");
  });
});

describe("the phases", () => {
  it("lists each phase with its Run, and locks them while running", () => {
    const stopped = renderToStaticMarkup(
      <PhasesCard
        construction={construction("stopped", "todo")}
        busy={false}
        onSave={noop}
        onRun={noop}
      />,
    );
    expect(stopped).toContain('value="Data model"');
    expect(stopped).toContain('value="Checkout"');
    expect(stopped).toMatch(/<\/svg>Run<\/button>/);
    expect(stopped).toContain("Add phase");

    const running = renderToStaticMarkup(
      <PhasesCard
        construction={construction("running", "running")}
        busy={false}
        onSave={noop}
        onRun={noop}
      />,
    );
    // Each saved phase opens its own page.
    expect(stopped).toContain('href="#/applications/a1/construct/phases/p1"');
    expect(stopped).toContain("Details");
    expect(running).not.toContain("Add phase");
    expect(running).toMatch(/<input[^>]*disabled[^>]*value="Data model"/);
  });

  it("marks the phase the run is on, with what it is doing", () => {
    const working = renderToStaticMarkup(
      <PhasesCard
        construction={construction("running", "running")}
        busy={false}
        onSave={noop}
        onRun={noop}
      />,
    );
    expect(working.match(/phase-working/g)).toHaveLength(1);
    expect(working).toContain("Working on “Books list”");

    // The phase reached with no tasks yet is having them planned.
    const reached = construction("running", "done");
    reached.phases[0].status = "done";
    const planning = renderToStaticMarkup(
      <PhasesCard construction={reached} busy={false} onSave={noop} onRun={noop} />,
    );
    expect(planning).toMatch(/phase phase-running phase-working[\s\S]*value="Checkout"/);
    expect(planning).toContain("Planning its tasks…");

    const stopped = renderToStaticMarkup(
      <PhasesCard
        construction={construction("stopped", "todo")}
        busy={false}
        onSave={noop}
        onRun={noop}
      />,
    );
    expect(stopped).not.toContain("phase-working");
  });
});

describe("the specification", () => {
  it("says why the last plan failed, with a link to its run", () => {
    const failed = {
      ...construction("draft", "todo"),
      phases: [],
      plan_run: "r9",
      error: "planning the phases failed: invalid x-api-key",
    };
    const html = renderToStaticMarkup(
      <SpecificationCard
        construction={failed}
        busy={false}
        onPlan={noop}
        onResearch={noop}
        onSave={noop}
      />,
    );
    expect(html).toContain("planning the phases failed: invalid x-api-key");
    expect(html).toContain('href="#/runs/r9"');
    expect(html).toContain("See the planning run");
  });
});

describe("the tabs", () => {
  it("numbers the steps, marks the current one and locks the ones not reached", () => {
    const draft = { ...construction("draft", "todo"), phases: [] };
    const html = renderToStaticMarkup(
      <StepTabs construction={draft} tab="specification" onPick={noop} />,
    );
    expect(html).toMatch(/bg-primary[^"]*">1<\/span>Specification/);
    expect(html).toMatch(/bg-secondary-lt">2<\/span>Research/);
    // The research opens once there is a specification to research.
    expect(html).not.toMatch(/disabled[^>]*>(?:(?!<\/a>).)*2<\/span>Research/);
    const empty = renderToStaticMarkup(
      <StepTabs construction={{ ...draft, specification: "" }} tab="specification" onPick={noop} />,
    );
    expect(empty).toMatch(/disabled[^>]*>.*2<\/span>Research/);
    expect(html).toMatch(/bg-secondary-lt">3<\/span>Phases/);
    expect(html).toMatch(/disabled[^>]*>.*3<\/span>Phases/);
    expect(html).toMatch(/disabled[^>]*>.*4<\/span>Progress/);
    expect(html).not.toMatch(/disabled[^>]*>.*1<\/span>Specification/);
  });
});

describe("a phase's own page", () => {
  it("shows each task in full, with the requirements folded under them", () => {
    const c = construction("stopped", "done");
    const [first] = c.phases;
    const phase = {
      ...first,
      description: "The tables.",
      tasks: [
        {
          ...first.tasks[0],
          id: "t0",
          title: "Tables `books`",
          brief: "Create `books` and `authors`.",
          summary: "Done.",
          created: [
            { kind: "table", name: "books" },
            { kind: "query", name: "listBooks" },
          ],
        },
        {
          ...first.tasks[0],
          status: "todo",
          runs: [],
          brief: "List the `books` newest first. Then the authors.",
          summary: null,
          depends_on: ["t0"],
        },
      ],
    };
    const html = renderToStaticMarkup(
      <PhaseDetail construction={c} phase={phase} busy={false} onRetry={noop} onSkip={noop} onPlan={noop} />,
    );
    expect(html).toContain("run shows what it was told and what it answered.");
    // The requirements are there to check against, folded under the tasks.
    expect(html.indexOf("Books list")).toBeLessThan(html.indexOf("What the phase was asked for"));
    expect(html).toMatch(/<details class="card run-context">/);
    expect(html).toContain("books and authors");
    expect(html).toMatch(/Tables <\/span><code>books<\/code>/);
    // Each task scans as one line of what came of it — the summary's first
    // sentence, else the brief's — with what it made and what it waits for.
    expect(html).toMatch(/pd-task-gist"><span>Done\.<\/span>/);
    expect(html).toMatch(
      /pd-task-gist"><span>List the <\/span><code>books<\/code><span> newest first\.<\/span><\/div>/,
    );
    // What it created: the new table as a link to it, the new query by name.
    expect(html).toMatch(/<a class="pd-created" href="#\/tables\/books" title="New table books"><svg[^]*?<\/svg>books<\/a>/);
    expect(html).toMatch(/<span class="pd-created" title="New query listBooks"><svg[^]*?<\/svg>listBooks<\/span>/);
    expect(html).toContain("after Tables books");
    // Its brief and summary are on its run's page; only the task not run yet
    // offers its brief here.
    expect(html.match(/>Brief<\/button>/g)).toHaveLength(1);
    // A finished task is edged green, one not run yet grey.
    expect(html).toContain('class="pd-task build-task-done"');
    expect(html).toContain("1 of 2 tasks");
  });

  it("says when a phase's tasks are not planned yet", () => {
    const c = construction("planned", "todo");
    const html = renderToStaticMarkup(
      <PhaseDetail construction={c} phase={c.phases[1]} busy={false} onRetry={noop} onSkip={noop} onPlan={noop} />,
    );
    expect(html).toContain("Not planned yet");
    // Planned against what the phase before it makes, so not before that.
    expect(html).not.toContain("Plan tasks");
  });

  it("offers to plan the next phase's tasks, and to plan them again before they run", () => {
    const c = construction("planned", "todo");
    const next = { ...c, phases: [c.phases[1]] };
    const html = renderToStaticMarkup(
      <PhaseDetail construction={next} phase={next.phases[0]} busy={false} onRetry={noop} onSkip={noop} onPlan={noop} />,
    );
    expect(html).toContain("Plan tasks");
    expect(html).toContain("stops before they run");

    const planned = {
      ...c,
      phases: [{ ...c.phases[0], status: "planned", tasks: [{ ...c.phases[0].tasks[0], runs: [] }] }],
    };
    const again = renderToStaticMarkup(
      <PhaseDetail construction={planned} phase={planned.phases[0]} busy={false} onRetry={noop} onSkip={noop} onPlan={noop} />,
    );
    expect(again).toContain("Plan again");
    // Not once a task has run: what ran stays recorded.
    const ran = renderToStaticMarkup(
      <PhaseDetail construction={c} phase={c.phases[0]} busy={false} onRetry={noop} onSkip={noop} onPlan={noop} />,
    );
    expect(ran).not.toContain("Plan again");
  });
});

describe("the mockups", () => {
  const screens = () => [
    { name: "Orders list", file: "orders-list.html" },
    { name: "Order form", file: "order-form.html" },
  ];
  // The screens' HTML is fetched after the first render, which is all a
  // static render shows.
  const loadScreens = async () => [{ html: "<p></p>" }, { html: "<p></p>" }];
  const withMockups = {
    ...construction("draft", "todo"),
    phases: [],
    mockups: [
      { id: "m1", round: 1, title: "Table", rationale: "Rows.", screens: screens(), status: "chosen", feedback: "denser" },
      { id: "m2", round: 2, title: "Cards", rationale: "A card per order.", screens: screens(), status: "proposed", feedback: null },
      { id: "m3", round: 2, title: "Split view", rationale: "", screens: screens(), status: "proposed", feedback: null },
    ],
  };

  it("shows the last round's screens in frames that run nothing", () => {
    const html = renderToStaticMarkup(
      <MockupsCard construction={withMockups} busy={false} onRevise={noop} onChoose={noop} onPlan={noop} onPropose={noop} loadScreens={loadScreens} />,
    );
    expect(html).toContain("Cards");
    expect(html).toContain("Split view");
    // The two are alternatives, numbered as such, with a way to each.
    expect(html).toContain("2 alternatives");
    expect(html).toContain("Mockup 1 of 2");
    expect(html).toContain("Mockup 2 of 2");
    expect(html).toMatch(/class="mockup-number"[^>]*>2</);
    expect(html).toContain('id="mockup-m3"');
    // The screens' HTML comes after, by mockup: until then, its place.
    expect(html).toContain("mockup-frame-loading");
    // Every screen of a mockup is in view as a miniature, the first picked.
    expect(html.match(/class="mockup-thumb( active)?"/g)?.length).toBe(4);
    expect(html).toMatch(/class="mockup-thumb active"[^>]*>.*?Orders list/);
    expect(html).toContain("Screen 1 of 2");
    expect(html).toContain("2 screens");
    expect(html).toContain('aria-label="Next screen"');
    // The first round is listed below, with the chosen one marked.
    expect(html).toContain("Earlier rounds (1)");
    expect(html).toContain("from an earlier round");
    expect(html).toMatch(/>Plan phases<\/button>/);
    // Both widths, once there is something to show at them.
    expect(html).toContain(">Phone</button>");
    // A new round comes from revising, not from proposing again.
    expect(html).not.toContain("Propose mockups</button>");
  });

  it("offers the same planning when no mockup is chosen, asked to confirm", () => {
    const none = {
      ...withMockups,
      mockups: withMockups.mockups.map((m) => ({ ...m, status: "proposed" })),
    };
    const html = renderToStaticMarkup(
      <MockupsCard construction={none} busy={false} onRevise={noop} onChoose={noop} onPlan={noop} onPropose={noop} loadScreens={loadScreens} />,
    );
    expect(html).toMatch(/>Plan phases<\/button>/);
    expect(html).toContain("Choose mockup 1");
    expect(html).toContain("Choose mockup 2");
  });

  it("offers planning again once the phases are planned", () => {
    const planned = {
      ...withMockups,
      status: "planned",
      phases: construction("planned", "todo").phases,
    };
    const html = renderToStaticMarkup(
      <MockupsCard construction={planned} busy={false} onRevise={noop} onChoose={noop} onPlan={noop} onPropose={noop} loadScreens={loadScreens} />,
    );
    expect(html).toMatch(/>Plan phases again<\/button>/);
    // Choosing another stays possible.
    const button = html.match(/<button[^>]*>(?:(?!<\/button>).)*Choose mockup 1<\/button>/)?.[0];
    expect(button).toBeDefined();
    expect(button).not.toContain("disabled");
  });

  it("offers only the research or planning on the specification", () => {
    const draft = { ...construction("draft", "todo"), phases: [] };
    const html = renderToStaticMarkup(
      <SpecificationCard construction={draft} busy={false} onPlan={noop} onResearch={noop} onSave={noop} />,
    );
    expect(html).toMatch(/btn-primary[^>]*>(?:(?!<\/button>).)*Continue to research/);
    expect(html).toContain("Plan phases");
    expect(html).not.toContain("Ask questions");
    expect(html).not.toContain("Propose mockups");
    expect(html).toContain("Research is optional");
    // Once there are phases, planning them again is the suggested step.
    const planned = { ...construction("planned", "todo") };
    const again = renderToStaticMarkup(
      <SpecificationCard construction={planned} busy={false} onPlan={noop} onResearch={noop} onSave={noop} />,
    );
    expect(again).toMatch(/btn-primary[^>]*>(?:(?!<\/button>).)*Plan phases again/);
  });

  it("offers a first round when there are none yet", () => {
    const none = { ...withMockups, mockups: [] };
    const html = renderToStaticMarkup(
      <MockupsCard construction={none} busy={false} onRevise={noop} onChoose={noop} onPlan={noop} onPropose={noop} loadScreens={loadScreens} />,
    );
    expect(html).toContain("Propose mockups");
    expect(html).not.toContain("alternatives");
    // The second part of the research, after the questions.
    expect(html).toMatch(/mockup-number-sm me-2">2<\/span>Mockups/);
    expect(html).toContain("Ask the questions first");
    // No widths to pick before there is anything to show.
    expect(html).not.toContain(">Phone</button>");
    // Proposing is the card's action, planning comes under it.
    expect(html.indexOf("Propose mockups</button>")).toBeGreaterThan(0);
    expect(html.indexOf("Propose mockups</button>")).toBeLessThan(html.indexOf("card-body"));
    expect(html).toMatch(/<\/div><div class="research-plan"><button[^>]*>.*Plan phases<\/button>/);
  });
});

describe("the questions", () => {
  const draft = { ...construction("draft", "todo"), phases: [] };

  it("offers to ask when none are asked yet", () => {
    const html = renderToStaticMarkup(
      <QuestionsCard construction={draft} busy={false} onAsk={noop} onAnswer={noop} />,
    );
    expect(html).toMatch(/btn-primary[^>]*>(?:(?!<\/button>).)*Ask questions/);
    expect(html).toContain("leaves open");
    expect(html).toMatch(/mockup-number-sm me-2">1<\/span>Questions/);
  });

  it("numbers the questions, shows the saved answers and how many are answered", () => {
    const asked = {
      ...draft,
      questions: [
        { id: "q1", text: "Who takes the orders?", answer: "The sales team" },
        { id: "q2", text: "Which currency?", answer: null },
      ],
    };
    const html = renderToStaticMarkup(
      <QuestionsCard construction={asked} busy={false} onAsk={noop} onAnswer={noop} />,
    );
    expect(html).toMatch(/class="mockup-number mockup-number-sm"[^>]*>2</);
    expect(html).toContain("Who takes the orders?");
    expect(html).toContain("The sales team</textarea>");
    expect(html).toContain('id="question-q2"');
    expect(html).toContain("1 of 2 answered");
    expect(html).toContain("Ask again");
    // Nothing changed yet: nothing to save.
    const save = html.match(/<button[^>]*>(?:(?!<\/button>).)*Save answers<\/button>/)?.[0];
    expect(save).toContain("disabled");
  });

  it("says the agent is writing them, and locks them meanwhile", () => {
    const asking = {
      ...draft,
      asking: true,
      questions: [{ id: "q1", text: "Who?", answer: null }],
    };
    const html = renderToStaticMarkup(
      <QuestionsCard construction={asking} busy={false} onAsk={noop} onAnswer={noop} />,
    );
    expect(html).toContain("writing its questions");
    expect(html).toMatch(/<textarea[^>]*disabled/);
  });
});
