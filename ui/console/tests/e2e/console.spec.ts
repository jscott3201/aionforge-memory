import { expect, type Page, test } from "@playwright/test";

function collectRuntimeErrors(page: Page): string[] {
  const errors: string[] = [];
  page.on("console", (message) => {
    if (message.type() === "error") {
      errors.push(message.text());
    }
  });
  page.on("pageerror", (error) => {
    errors.push(error.message);
  });
  return errors;
}

test("renders the operator dashboard at /console", async ({ page }) => {
  const errors = collectRuntimeErrors(page);

  await page.goto("/console");

  await expect(page).toHaveTitle("Aionforge Memory Console");
  await expect(
    page.getByRole("heading", { name: "Operator dashboard" }),
  ).toBeVisible();
  await expect(page.getByText("Base path")).toBeVisible();
  await expect(page.getByLabel("Console base")).toContainText("/console");
  await expect(errors).toEqual([]);
});

test("supports static SPA deep links and basic controls", async ({ page }) => {
  const errors = collectRuntimeErrors(page);

  await page.goto("/console/records");

  await expect(page.getByRole("heading", { name: "Records" })).toBeVisible();
  await expect(
    page.getByText("Search-backed records with read_memory detail panes."),
  ).toBeVisible();

  const search = page.getByLabel("Search memory");
  if (await search.isVisible()) {
    await search.fill("audit");
    await expect(search).toHaveValue("audit");
  }

  const root = page.locator("html");
  const initialTheme = await root.getAttribute("data-theme");
  await page.getByRole("button", { name: "Toggle theme" }).click();
  await expect(root).toHaveAttribute(
    "data-theme",
    initialTheme === "dark" ? "light" : "dark",
  );
  await expect(errors).toEqual([]);
});
