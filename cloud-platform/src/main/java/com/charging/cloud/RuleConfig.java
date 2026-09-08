package com.charging.cloud;
import jakarta.validation.constraints.Max; import jakarta.validation.constraints.Min; import jakarta.validation.constraints.NotNull; import jakarta.validation.constraints.Pattern;
public record RuleConfig(@Min(0) @Max(604800) long unchargedGraceSeconds, @Min(0) @Max(604800) long completedGraceSeconds, @Min(0) @Max(604800) long vehicleEnterConfirmSeconds, @Min(0) @Max(604800) long vehicleLeaveConfirmSeconds, @Min(0) @Max(604800) long alarmRepeatSeconds, @Min(0) @Max(604800) long alarmAutoStopSeconds, @NotNull @Pattern(regexp="ALLOW|WAIT|VIOLATE") String unknownVehicleAction, @NotNull @Pattern(regexp="ALLOW|WAIT|VIOLATE") String unknownStatusAction, @NotNull @Pattern(regexp="ALLOW|WAIT|VIOLATE") String faultStatusAction) {
  public static RuleConfig defaults() { return new RuleConfig(600,900,3,5,60,300,"WAIT","WAIT","ALLOW"); }
}
