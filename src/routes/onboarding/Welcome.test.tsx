import { describe, it, expect } from "vitest";
import { render, screen } from "@testing-library/react";
import { MemoryRouter } from "react-router-dom";
import { Welcome } from "./Welcome";

describe("Welcome", () => {
  it("renders the two onboarding entry options", () => {
    render(
      <MemoryRouter>
        <Welcome />
      </MemoryRouter>,
    );
    expect(screen.getByRole("button", { name: /create new brain/i })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: /open existing brain/i })).toBeInTheDocument();
  });

  it("uses an accessible heading describing the welcome step", () => {
    render(
      <MemoryRouter>
        <Welcome />
      </MemoryRouter>,
    );
    expect(screen.getByRole("heading", { name: /welcome to brain/i })).toBeInTheDocument();
  });
});
