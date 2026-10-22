const key = "dsh-preferred-profile";
export function preferredDshProfile() {
  try {
    return localStorage.getItem(key) || "desktop";
  } catch {
    return "desktop";
  }
}
export function rememberDshProfile(profile: string) {
  try {
    localStorage.setItem(key, profile);
  } catch {
    /* Storage may be unavailable. */
  }
}
