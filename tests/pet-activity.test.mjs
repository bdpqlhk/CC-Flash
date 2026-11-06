import { test } from "node:test";
import assert from "node:assert/strict";
import { createPetActivityTracker } from "../src/pet-activity.js";

const message = id => ({ id, timestamp: 1000 + id });
const file = (name, size = 1, modified = 1000) => ({ name, size, modified });

test("existing history establishes a baseline without notifying", () => {
  const activity = createPetActivityTracker();
  assert.equal(activity.observeMessages([message(20)]), false);
  assert.equal(activity.observeMessages([message(20)]), false);
  assert.equal(activity.observeFiles([file("existing.txt")]), false);
  assert.equal(activity.observeFiles([file("existing.txt")]), false);
});

test("the first message after an empty baseline notifies", () => {
  const activity = createPetActivityTracker();
  assert.equal(activity.observeMessages([]), false);
  assert.equal(activity.observeMessages([message(1)]), true);
  assert.equal(activity.observeMessages([message(1)]), false);
});

test("messages still notify when the server's 200-message buffer is full", () => {
  const activity = createPetActivityTracker();
  const history = Array.from({ length: 200 }, (_, i) => message(i + 1));
  assert.equal(activity.observeMessages(history), false);
  assert.equal(activity.observeMessages([...history.slice(1), message(201)]), true);
  assert.equal(activity.observeMessages([...history.slice(2), message(201), message(202)]), true);
});

test("reconnect discovers new messages without replaying unchanged history", () => {
  const activity = createPetActivityTracker();
  activity.observeMessages([message(10)]);
  assert.equal(activity.observeMessages([message(20)]), true);
  assert.equal(activity.observeMessages([message(20)]), false);
  assert.equal(activity.observeMessages([]), false);
  assert.equal(activity.observeMessages([message(1)]), true);
});

test("new uploads notify, deletions and reordered lists do not", () => {
  const activity = createPetActivityTracker();
  activity.observeFiles([file("a.txt"), file("b.txt")]);
  assert.equal(activity.observeFiles([file("b.txt"), file("a.txt")]), false);
  assert.equal(activity.observeFiles([file("b.txt")]), false);
  assert.equal(activity.observeFiles([file("b.txt"), file("phone.txt")]), true);
  assert.equal(activity.observeFiles([file("b.txt"), file("phone.txt")]), false);
});

test("same-name changes and new files with an unchanged total count notify", () => {
  const activity = createPetActivityTracker();
  activity.observeFiles([file("a.txt")]);
  assert.equal(activity.observeFiles([file("a.txt", 2)]), true);
  assert.equal(activity.observeFiles([file("a.txt", 2, 2000)]), true);
  assert.equal(activity.observeFiles([file("b.txt", 2, 2000)]), true);
});
