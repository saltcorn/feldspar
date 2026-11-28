// The JSON the module's routes answer, as the admin API's construction
// responses had it (ui/admin/src/client.ts: GetConstructionResponse,
// SavePhasesRequest, GetMockupScreensResponse). One difference: a phase's,
// task's, mockup's, question's, feedback's and error's id is a row id of the
// module's tables, a number, where the admin API's were uuids. Ids that come
// from a URL are text, so they are compared with `sameId`.

/** A row id of the module's tables: a number in JSON, text in a URL. */
export type Id = number | string;

/** Whether two ids name the same row, whichever of them is text. */
export function sameId(
  a: Id | null | undefined,
  b: Id | null | undefined
): boolean {
  return (
    a !== null &&
    a !== undefined &&
    b !== null &&
    b !== undefined &&
    String(a) === String(b)
  );
}

export type Requirement = { text: string; priority: string };

export type ConstructionTask = {
  id: Id;
  /** The task's own name in its plan, which `depends_on` may name it by. */
  uid?: string;
  kind: string;
  title: string;
  brief: string;
  depends_on: Array<Id>;
  status: string;
  runs: Array<string>;
  summary?: string | null;
  created: Array<unknown>;
  feedback?: Id | null;
};

export type ConstructionPhase = {
  id: Id;
  title: string;
  description: string;
  requirements: Array<Requirement>;
  status: string;
  tasks: Array<ConstructionTask>;
};

export type ConstructionQuestion = {
  id: Id;
  text: string;
  answer?: string | null;
};

export type ConstructionMockup = {
  id: Id;
  round: number;
  title: string;
  rationale: string;
  screens: Array<{ name: string; file: string }>;
  status: string;
  feedback?: string | null;
};

export type ConstructionFeedback = {
  id: Id;
  phase: Id;
  title: string;
  description: string;
  requirements: Array<Requirement>;
  run?: string | null;
  planning: boolean;
  status: string;
};

export type ConstructionErrorEntry = {
  id: Id;
  construction: Id;
  at: string;
  source: string;
  phase?: Id | null;
  task?: Id | null;
  feedback?: Id | null;
  run?: string | null;
  construction_run?: Id | null;
  message: string;
  current: boolean;
};

export type Construction = {
  id: Id;
  application: string;
  status: string;
  agent?: string | null;
  specification: string;
  started_by?: string | null;
  plan_run?: string | null;
  planning: boolean;
  error?: string | null;
  until_phase?: Id | null;
  plan_only: boolean;
  designing: boolean;
  asking: boolean;
  questions: Array<ConstructionQuestion>;
  mockups: Array<ConstructionMockup>;
  feedback: Array<ConstructionFeedback>;
  errors: Array<ConstructionErrorEntry>;
  updated_at?: string | null;
  phases: Array<ConstructionPhase>;
};

/** A phase as `POST …/phases` takes it: an id for a saved one, none for a new one. */
export type PhaseDraft = {
  id?: Id | null;
  title: string;
  description: string;
  requirements: Array<Requirement>;
};

/** `GET …/mockups/:mockup/screens`. */
export type MockupScreens = Array<{ name: string; file: string; html: string }>;
