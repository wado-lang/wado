// A task tracker core: domain types, an event bus, a repository with an
// in-memory store, a query builder, and a scheduler.

export enum Priority {
  Low = 1,
  Normal = 2,
  High = 3,
  Urgent = 4,
}

export enum Status {
  Todo = "todo",
  InProgress = "in_progress",
  Blocked = "blocked",
  Done = "done",
}

export interface Identified {
  readonly id: string;
}

export interface Timestamped {
  createdAt: Date;
  updatedAt?: Date;
}

export interface Task extends Identified, Timestamped {
  title: string;
  description: string;
  status: Status;
  priority: Priority;
  assignee: string | null;
  tags: string[];
  estimateHours: number;
  dependsOn: string[];
}

export interface User extends Identified {
  name: string;
  email: string;
  capacityHours: number;
}

export type Predicate<T> = (item: T) => boolean;
export type Comparator<T> = (a: T, b: T) => number;
export type Listener<T> = (payload: T) => void;

export interface TaskEvents {
  created: Task;
  updated: { before: Task; after: Task };
  deleted: string;
}

export class EventBus<E> {
  private listeners: Map<keyof E, Array<Listener<any>>> = new Map();

  on<K extends keyof E>(event: K, listener: Listener<E[K]>): () => void {
    const list = this.listeners.get(event) || [];
    list.push(listener);
    this.listeners.set(event, list);
    return () => this.off(event, listener);
  }

  off<K extends keyof E>(event: K, listener: Listener<E[K]>): void {
    const list = this.listeners.get(event);
    if (!list) {
      return;
    }
    const index = list.indexOf(listener);
    if (index >= 0) {
      list.splice(index, 1);
    }
  }

  emit<K extends keyof E>(event: K, payload: E[K]): number {
    const list = this.listeners.get(event) || [];
    for (const listener of list.slice()) {
      listener(payload);
    }
    return list.length;
  }
}

export class NotFoundError extends Error {
  readonly entity: string;
  readonly key: string;

  constructor(entity: string, key: string) {
    super(`${entity} ${key} not found`);
    this.name = "NotFoundError";
    this.entity = entity;
    this.key = key;
  }
}

export class ValidationError extends Error {
  readonly problems: string[];

  constructor(problems: string[]) {
    super(`invalid: ${problems.join("; ")}`);
    this.name = "ValidationError";
    this.problems = problems;
  }
}

export abstract class Repository<T extends Identified> {
  protected items: Map<string, T> = new Map<string, T>();

  abstract validate(item: T): string[];

  get size(): number {
    return this.items.size;
  }

  find(id: string): T | undefined {
    return this.items.get(id);
  }

  get(id: string): T {
    const item = this.items.get(id);
    if (item === undefined) {
      throw new NotFoundError(this.constructor.name, id);
    }
    return item;
  }

  save(item: T): T {
    const problems = this.validate(item);
    if (problems.length > 0) {
      throw new ValidationError(problems);
    }
    this.items.set(item.id, item);
    return item;
  }

  remove(id: string): boolean {
    return this.items.delete(id);
  }

  all(): T[] {
    return Array.from(this.items.values());
  }

  where(predicate: Predicate<T>): T[] {
    return this.all().filter(predicate);
  }
}

export class UserRepository extends Repository<User> {
  validate(user: User): string[] {
    const problems: string[] = [];
    if (!/^[^@\s]+@[^@\s]+\.[a-z]{2,}$/i.test(user.email)) {
      problems.push(`bad email: ${user.email}`);
    }
    if (user.capacityHours <= 0 || user.capacityHours > 60) {
      problems.push("capacity must be within (0, 60]");
    }
    return problems;
  }
}

export class TaskRepository extends Repository<Task> {
  readonly events = new EventBus<TaskEvents>();
  private sequence = 0;
  private readonly clock: () => Date;

  constructor(clock: () => Date = () => new Date()) {
    super();
    this.clock = clock;
  }

  nextId(): string {
    this.sequence += 1;
    return "T-" + this.sequence.toString(36).toUpperCase().padStart(4, "0");
  }

  validate(task: Task): string[] {
    const problems: string[] = [];
    if (task.title.trim().length === 0) {
      problems.push("title is required");
    }
    if (task.estimateHours < 0) {
      problems.push("estimate cannot be negative");
    }
    for (const dep of task.dependsOn) {
      if (dep === task.id) {
        problems.push("a task cannot depend on itself");
      } else if (!this.items.has(dep)) {
        problems.push(`unknown dependency ${dep}`);
      }
    }
    return problems;
  }

  create(title: string, fields: Partial<Task> = {}): Task {
    const now = this.clock();
    const task: Task = {
      id: this.nextId(),
      title: title,
      description: fields.description || "",
      status: fields.status || Status.Todo,
      priority: fields.priority || Priority.Normal,
      assignee: fields.assignee || null,
      tags: fields.tags || [],
      estimateHours: fields.estimateHours || 1,
      dependsOn: fields.dependsOn || [],
      createdAt: now,
    };
    this.save(task);
    this.events.emit("created", task);
    return task;
  }

  update(id: string, patch: Partial<Task>): Task {
    const before = this.get(id);
    const after: Task = { ...before, ...patch, id: before.id, updatedAt: this.clock() };
    this.save(after);
    this.events.emit("updated", { before: before, after: after });
    return after;
  }

  delete(id: string): void {
    if (this.remove(id)) {
      for (const task of this.all()) {
        if (task.dependsOn.indexOf(id) >= 0) {
          this.update(task.id, { dependsOn: task.dependsOn.filter((d) => d !== id) });
        }
      }
      this.events.emit("deleted", id);
    }
  }
}

export class Query<T> {
  private filters: Array<Predicate<T>> = [];
  private order: Comparator<T> | null = null;
  private limitCount = Infinity;
  private offsetCount = 0;

  private readonly source: () => T[];

  constructor(source: () => T[]) {
    this.source = source;
  }

  filter(predicate: Predicate<T>): this {
    this.filters.push(predicate);
    return this;
  }

  sortBy<K extends keyof T>(key: K, direction: "asc" | "desc" = "asc"): this {
    const sign = direction === "asc" ? 1 : -1;
    this.order = (a, b) => {
      const x = a[key];
      const y = b[key];
      return x < y ? -sign : x > y ? sign : 0;
    };
    return this;
  }

  limit(count: number): this {
    this.limitCount = count;
    return this;
  }

  offset(count: number): this {
    this.offsetCount = count;
    return this;
  }

  run(): T[] {
    let rows = this.source().filter((row) => this.filters.every((f) => f(row)));
    if (this.order !== null) {
      rows = rows.sort(this.order);
    }
    return rows.slice(this.offsetCount, this.offsetCount + this.limitCount);
  }

  count(): number {
    return this.source().filter((row) => this.filters.every((f) => f(row))).length;
  }
}

export function groupBy<T, K extends string | number>(items: T[], key: (item: T) => K): Record<K, T[]> {
  const groups = {} as Record<K, T[]>;
  for (const item of items) {
    const k = key(item);
    (groups[k] = groups[k] || []).push(item);
  }
  return groups;
}

export function isTask(value: unknown): value is Task {
  return typeof value === "object" && value !== null && "title" in value && "status" in value;
}

export function topologicalOrder(tasks: Task[]): string[] {
  const indegree = new Map<string, number>();
  const edges = new Map<string, string[]>();
  const present = new Set(tasks.map((t) => t.id));
  const pending = (t: Task) => t.dependsOn.filter((d) => present.has(d));
  for (const task of tasks) {
    const deps = pending(task);
    indegree.set(task.id, deps.length);
    for (const dep of deps) {
      const list = edges.get(dep) || [];
      list.push(task.id);
      edges.set(dep, list);
    }
  }
  const ready: string[] = tasks.filter((t) => pending(t).length === 0).map((t) => t.id);
  const order: string[] = [];
  while (ready.length > 0) {
    const id = ready.shift() as string;
    order.push(id);
    for (const next of edges.get(id) || []) {
      const remaining = (indegree.get(next) || 0) - 1;
      indegree.set(next, remaining);
      if (remaining === 0) {
        ready.push(next);
      }
    }
  }
  if (order.length !== tasks.length) {
    throw new Error("dependency cycle detected");
  }
  return order;
}

export interface Assignment {
  taskId: string;
  userId: string;
  day: number;
}

export class Scheduler {
  private readonly tasks: TaskRepository;
  private readonly users: UserRepository;
  private readonly hoursPerDay: number;

  constructor(tasks: TaskRepository, users: UserRepository, hoursPerDay: number = 6) {
    this.tasks = tasks;
    this.users = users;
    this.hoursPerDay = hoursPerDay;
  }

  plan(): Assignment[] {
    const order = topologicalOrder(this.tasks.where((t) => t.status !== Status.Done));
    const load = new Map<string, number>();
    const finishDay = new Map<string, number>();
    const result: Assignment[] = [];
    const people = this.users.all().sort((a, b) => b.capacityHours - a.capacityHours);
    if (people.length === 0) {
      return result;
    }
    for (const id of order) {
      const task = this.tasks.get(id);
      let earliest = 0;
      for (const dep of task.dependsOn) {
        earliest = Math.max(earliest, finishDay.get(dep) || 0);
      }
      let best = people[0];
      let bestLoad = Number.POSITIVE_INFINITY;
      for (const person of people) {
        const current = load.get(person.id) || 0;
        const preferred = task.assignee === person.id ? -1000 : 0;
        if (current + preferred < bestLoad) {
          best = person;
          bestLoad = current + preferred;
        }
      }
      const used = load.get(best.id) || 0;
      const start = Math.max(earliest, Math.floor(used / this.hoursPerDay));
      const days = Math.ceil(task.estimateHours / Math.min(this.hoursPerDay, best.capacityHours / 5));
      load.set(best.id, used + task.estimateHours);
      finishDay.set(id, start + days);
      result.push({ taskId: id, userId: best.id, day: start });
    }
    return result;
  }
}

export async function retry<T>(operation: () => Promise<T>, attempts: number = 3, delayMs: number = 50): Promise<T> {
  let lastError: unknown;
  for (let attempt = 1; attempt <= attempts; attempt++) {
    try {
      return await operation();
    } catch (error) {
      lastError = error;
      if (attempt < attempts) {
        await new Promise<void>((resolve) => setTimeout(resolve, delayMs * 2 ** (attempt - 1)));
      }
    }
  }
  throw lastError;
}

export function formatDuration(hours: number): string {
  if (hours < 1) {
    return `${Math.round(hours * 60)}m`;
  }
  const days = Math.floor(hours / 8);
  const rest = hours % 8;
  switch (true) {
    case days === 0:
      return `${rest}h`;
    case rest === 0:
      return `${days}d`;
    default:
      return `${days}d ${rest}h`;
  }
}

export function summarize(repo: TaskRepository): string {
  const byStatus = groupBy(repo.all(), (t) => t.status);
  const lines: string[] = [];
  for (const status of [Status.Todo, Status.InProgress, Status.Blocked, Status.Done]) {
    const tasks = byStatus[status] || [];
    const hours = tasks.reduce((sum, t) => sum + t.estimateHours, 0);
    lines.push(`${status.padEnd(12)}${String(tasks.length).padStart(3)}  ${formatDuration(hours)}`);
  }
  return lines.join("\n");
}

async function main(): Promise<void> {
  const users = new UserRepository();
  users.save({ id: "ana", name: "Ana", email: "ana@example.com", capacityHours: 32 });
  users.save({ id: "kenji", name: "Kenji", email: "kenji@example.com", capacityHours: 40 });

  let tick = Date.UTC(2026, 0, 5);
  const tasks = new TaskRepository(() => new Date((tick += 60000)));
  const log: string[] = [];
  const unsubscribe = tasks.events.on("created", (task) => log.push(`+ ${task.id} ${task.title}`));
  tasks.events.on("updated", ({ before, after }) => {
    if (before.status !== after.status) {
      log.push(`~ ${after.id} ${before.status} -> ${after.status}`);
    }
  });

  const schema = tasks.create("Design schema", { priority: Priority.High, estimateHours: 6, tags: ["db"] });
  const api = tasks.create("Build API", { dependsOn: [schema.id], estimateHours: 16, assignee: "kenji" });
  const ui = tasks.create("Build UI", { dependsOn: [api.id], estimateHours: 12, tags: ["frontend", "ux"] });
  tasks.create("Write docs", { priority: Priority.Low, estimateHours: 4, dependsOn: [api.id, ui.id] });
  unsubscribe();
  tasks.update(schema.id, { status: Status.Done });
  tasks.update(api.id, { status: Status.InProgress });

  const urgent = new Query(() => tasks.all())
    .filter((t) => t.priority >= Priority.Normal)
    .filter((t) => t.status !== Status.Done)
    .sortBy("estimateHours", "desc")
    .limit(5)
    .run();

  const plan = new Scheduler(tasks, users).plan();
  const checked = await retry(async () => plan.length, 2, 1);

  console.log(log.join("\n"));
  console.log(summarize(tasks));
  console.log(urgent.map((t) => t.title).join(", "));
  for (const { taskId, userId, day } of plan) {
    console.log(`day ${day}: ${userId} -> ${taskId}`);
  }
  console.log(`planned ${checked} tasks; isTask(schema) = ${isTask(schema)}`);
}

main().catch((error: Error) => {
  console.error(error.message);
});
