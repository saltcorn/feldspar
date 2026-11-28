// What the admin lends the screen when it mounts it (TODO-app-constructor
// AC12.4): the application, the rest of the URL, links within the screen, and
// requests to the module's routes with the admin's session and CSRF header.

export type Application = {
  id: string;
  name: string;
  subdomain: string;
  description: string;
  framework: { name: string };
  attributes?: unknown;
};

export type Host = {
  app: Application | null;
  path: string;
  href: (path: string) => string;
  navigate: (path: string) => void;
  routes: string;
  fetch: (path: string, init?: RequestInit) => Promise<Response>;
  json: (path: string, init?: RequestInit) => Promise<unknown>;
};
