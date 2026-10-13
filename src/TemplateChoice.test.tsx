import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { expect, it, vi } from "vitest";
import TemplateChoice, { type PreparedTemplate } from "./TemplateChoice";

const template: PreparedTemplate = {
  ready: false,
  token: "fixture-template-token",
  rows: [
    { id: 0, name: "Fixture first prototype" },
    { id: 7, name: "Fixture second prototype" },
  ],
};

it("requires an explicit single prototype and submits only its numeric id", async () => {
  const user = userEvent.setup();
  const select = vi.fn();
  render(<TemplateChoice template={template} busy={false} select={select} />);

  expect(
    screen.getByRole("group", { name: "选择一个账号作为导出原型" }),
  ).toBeInTheDocument();
  const first = screen.getByRole("radio", { name: "Fixture first prototype" });
  const second = screen.getByRole("radio", {
    name: "Fixture second prototype",
  });
  const submit = screen.getByRole("button", { name: "使用此原型" });
  expect(first).not.toBeChecked();
  expect(second).not.toBeChecked();
  expect(submit).toBeDisabled();

  await user.click(first);
  expect(first).toBeChecked();
  expect(submit).toBeEnabled();
  expect(select).not.toHaveBeenCalled();
  await user.click(second);
  expect(first).not.toBeChecked();
  expect(second).toBeChecked();
  await user.click(submit);
  expect(select.mock.calls).toEqual([[7]]);
});

it("accepts a zero id and blocks changes and submission while busy", async () => {
  const user = userEvent.setup();
  const select = vi.fn();
  const { rerender } = render(
    <TemplateChoice template={template} busy={false} select={select} />,
  );
  const first = screen.getByRole("radio", { name: "Fixture first prototype" });
  const second = screen.getByRole("radio", {
    name: "Fixture second prototype",
  });
  const submit = screen.getByRole("button", { name: "使用此原型" });
  await user.click(first);

  rerender(<TemplateChoice template={template} busy select={select} />);
  expect(first).toBeDisabled();
  expect(second).toBeDisabled();
  expect(submit).toBeDisabled();
  await user.click(second);
  await user.click(submit);
  expect(first).toBeChecked();
  expect(select).not.toHaveBeenCalled();

  rerender(<TemplateChoice template={template} busy={false} select={select} />);
  await user.click(submit);
  expect(select.mock.calls).toEqual([[0]]);
});
