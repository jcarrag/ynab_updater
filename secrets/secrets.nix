# Create secret file
# > agenix -e secret1.age
# Update all secret files with new publicKeys
# > agenix -r

let
  fwk_james = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIHMKP2hPhz+L3GJ2eoj4DTtZbdgSm5cS+RVtV9lY7fpB james@carragher.dev";
  lunar-fwk_james = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAID+kfnnvuaVqRuhUPpPlUY4s7UPMkoI9vGskJxep0ZPa james@carragher.dev";
  hm90_james = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIIwJ3qvOGZRCgxKwe9TghG03MyM2eYWLy3wmjVK23T+M james@carragher.dev";
  trusted-systems = [
    hm90_james
    fwk_james
    lunar-fwk_james
  ];
in
{
  "settings.toml.age" = {
    publicKeys = trusted-systems;
    armor = true;
  };
}
